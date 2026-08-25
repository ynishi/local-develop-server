use std::path::PathBuf;

use anyhow::{Context, Result};
use journal_mcp_rmcp::{JournalMcpServer, RunConfig};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResult, ListResourcesResult, PaginatedRequestParams,
        ReadResourceRequestParams, ReadResourceResult, Tool,
    },
    service::{Peer, RequestContext, RoleClient, RunningService},
    transport::streamable_http_client::{
        StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
    },
};
use tokio::sync::OnceCell;

/// Prefix that every journal-mcp tool name carries in-crate.
///
/// Used by [`JournalModule::try_call`] to decide whether an incoming lds
/// tool call should be forwarded to the backend. Kept public so downstream
/// code (routing tables, tests, docs) can reference the exact string
/// without duplicating the literal.
pub const JOURNAL_TOOL_PREFIX: &str = "journal_";

/// Where journal tool calls actually execute.
enum Backend {
    /// In-process `JournalMcpServer` over the session-local EventLog
    /// (`{session_root}/workspace/.journal.db`, default).
    Embed {
        server: JournalMcpServer,
        file_projection_path: Option<PathBuf>,
    },
    /// Remote `journal-mcp --mcp-http` daemon (the SSOT host).
    Remote(RemoteBackend),
}

/// Lazily-connected streamable-HTTP client to the SSOT daemon.
///
/// Construction performs no I/O — the connection is established on the
/// first journal tool call and reused afterwards. This keeps
/// `session_start` synchronous and lock-friendly (its documented "no
/// `.await`, no upstream I/O" invariant), and means a daemon that is down
/// fails loudly *at call time* instead of silently degrading to a local
/// EventLog (two writable copies is the failure mode this mode exists to
/// remove — never fall back).
struct RemoteBackend {
    /// Endpoint URL, e.g. `https://my-journal.fly.dev/mcp`.
    url: String,
    /// Bearer token (raw value; the transport adds the `Bearer ` prefix).
    token: Option<String>,
    /// Remote project root injected into calls that omit `project_root`
    /// (e.g. `/data/journal/<repo-name>` on the daemon's volume). The
    /// daemon namespaces EventLogs by path, so without this every lds
    /// session would land in the daemon's startup default project.
    project_key: Option<String>,
    conn: OnceCell<RunningService<RoleClient, ()>>,
}

impl RemoteBackend {
    /// Connect on first use; subsequent calls reuse the running client.
    ///
    /// A failed attempt is not cached (`OnceCell::get_or_try_init`), so a
    /// daemon that comes up later heals on the next call.
    async fn peer(&self) -> Result<&Peer<RoleClient>, McpError> {
        let running = self
            .conn
            .get_or_try_init(|| async {
                let mut config = StreamableHttpClientTransportConfig::with_uri(self.url.clone());
                if let Some(token) = &self.token {
                    config = config.auth_header(token.clone());
                }
                let transport =
                    StreamableHttpClientTransport::with_client(reqwest::Client::default(), config);
                ().serve(transport).await.map_err(|e| {
                    McpError::internal_error(
                        format!("journal remote connect failed ({}): {e}", self.url),
                        None,
                    )
                })
            })
            .await?;
        Ok(running.peer())
    }
}

/// Map a client-side transport error onto the MCP error the lds caller sees.
fn remote_err(url: &str, e: impl std::fmt::Display) -> McpError {
    McpError::internal_error(format!("journal remote call failed ({url}): {e}"), None)
}

/// Journal module — either a thin wrapper around
/// `journal-mcp-rmcp::JournalMcpServer` (embed mode) or a streamable-HTTP
/// client to a central `journal-mcp --mcp-http` daemon (remote mode).
///
/// Both modes expose the same forwarding helpers that plug into lds's
/// `ServerHandler` composition points (`list_tools`, `call_tool`,
/// `list_resources`, `read_resource`). journal-mcp tools already carry the
/// `journal_` prefix in-crate, so no name rewriting is needed in either
/// mode. In remote mode, calls that omit `project_root` get the configured
/// `project_key` injected so the daemon addresses this project's EventLog
/// (an explicitly passed `project_root` always wins). Local `journal.md`
/// materialization in remote mode goes through the `journal_dump` tool —
/// the daemon returns the rendered Markdown and the caller writes the file.
pub struct JournalModule {
    backend: Backend,
}

impl JournalModule {
    /// Construct an embed-mode module rooted at `project_root` (typically
    /// `session.root().to_path_buf()`). Pass `Some(path)` in
    /// `file_projection` to attach a `FileProjection` at startup;
    /// `None` runs EventLog-only (attach/detach later via the
    /// `journal_projection_*` MCP tools).
    pub fn new(project_root: PathBuf, file_projection: Option<PathBuf>) -> Result<Self> {
        let cfg = RunConfig {
            project_root,
            file_projection: file_projection.clone(),
        };
        let server = JournalMcpServer::new(cfg).context("failed to construct JournalMcpServer")?;
        Ok(Self {
            backend: Backend::Embed {
                server,
                file_projection_path: file_projection,
            },
        })
    }

    /// Construct a remote-mode module targeting `url` (a streamable-HTTP
    /// MCP endpoint, e.g. `https://my-journal.fly.dev/mcp`).
    ///
    /// No I/O happens here — see [`RemoteBackend`] for the lazy-connect
    /// rationale. `token` is the raw bearer token (usually read from the
    /// env var named by `[remote.journal] token_env`); `project_key` is the
    /// remote project root injected into calls that omit `project_root`
    /// (usually `[remote.journal] project_key`).
    pub fn new_remote(url: String, token: Option<String>, project_key: Option<String>) -> Self {
        Self {
            backend: Backend::Remote(RemoteBackend {
                url,
                token,
                project_key,
                conn: OnceCell::new(),
            }),
        }
    }

    /// Underlying upstream server (mostly for tests and introspection).
    /// `None` in remote mode — there is no in-process server to inspect.
    pub fn server(&self) -> Option<&JournalMcpServer> {
        match &self.backend {
            Backend::Embed { server, .. } => Some(server),
            Backend::Remote(_) => None,
        }
    }

    /// Path of the startup-attached FileProjection, if any. Runtime
    /// attach/detach via `journal_projection_attach` /
    /// `journal_projection_detach` is not reflected here — this only
    /// reports the constructor-time value. Always `None` in remote mode
    /// (the daemon never writes files on the client's behalf; use
    /// `journal_dump` to materialize a local `journal.md`).
    pub fn file_projection_path(&self) -> Option<PathBuf> {
        match &self.backend {
            Backend::Embed {
                file_projection_path,
                ..
            } => file_projection_path.clone(),
            Backend::Remote(_) => None,
        }
    }

    /// List the journal tools verbatim (no prefix rewriting — journal-mcp
    /// tools already carry the `journal_` prefix in-crate).
    ///
    /// Consumers merge the result into their own `list_tools` response.
    pub async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<Vec<Tool>, McpError> {
        let listed = match &self.backend {
            Backend::Embed { server, .. } => {
                ServerHandler::list_tools(server, request, context).await?
            }
            Backend::Remote(remote) => remote
                .peer()
                .await?
                .list_tools(request)
                .await
                .map_err(|e| remote_err(&remote.url, e))?,
        };
        Ok(listed.tools)
    }

    /// If `request.name` begins with `journal_`, forward to the backend
    /// verbatim. Otherwise return `Ok(None)` so the caller can try the
    /// next dispatch layer.
    ///
    /// In remote mode, a configured `project_key` is injected as
    /// `project_root` when the call omits it (`journal_info` excluded — it
    /// takes no parameters and reports daemon-level state).
    pub async fn try_call(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<Option<CallToolResult>, McpError> {
        if !request.name.starts_with(JOURNAL_TOOL_PREFIX) {
            return Ok(None);
        }
        let result = match &self.backend {
            Backend::Embed { server, .. } => {
                ServerHandler::call_tool(server, request, context).await?
            }
            Backend::Remote(remote) => {
                let mut request = request;
                if let Some(key) = &remote.project_key
                    && request.name.as_ref() != "journal_info"
                {
                    let args = request.arguments.get_or_insert_with(Default::default);
                    if !args.contains_key("project_root") {
                        args.insert(
                            "project_root".to_string(),
                            serde_json::Value::String(key.clone()),
                        );
                    }
                }
                remote
                    .peer()
                    .await?
                    .call_tool(request)
                    .await
                    .map_err(|e| remote_err(&remote.url, e))?
            }
        };
        Ok(Some(result))
    }

    /// Forward `list_resources` verbatim.
    pub async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        match &self.backend {
            Backend::Embed { server, .. } => {
                ServerHandler::list_resources(server, request, context).await
            }
            Backend::Remote(remote) => remote
                .peer()
                .await?
                .list_resources(request)
                .await
                .map_err(|e| remote_err(&remote.url, e)),
        }
    }

    /// If `request.uri` begins with `journal-mcp://`, forward to the
    /// backend. Otherwise return `Ok(None)`.
    pub async fn try_read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<Option<ReadResourceResult>, McpError> {
        if !request.uri.starts_with("journal-mcp://") {
            return Ok(None);
        }
        let result = match &self.backend {
            Backend::Embed { server, .. } => {
                ServerHandler::read_resource(server, request, context).await?
            }
            Backend::Remote(remote) => remote
                .peer()
                .await?
                .read_resource(request)
                .await
                .map_err(|e| remote_err(&remote.url, e))?,
        };
        Ok(Some(result))
    }
}
