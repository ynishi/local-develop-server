use std::path::PathBuf;

use anyhow::{Context, Result};
use outline_mcp_rmcp::OutlineMcpServer;
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

/// Prefix applied to every outline tool name when surfaced from lds.
///
/// Kept public so downstream code (routing tables, tests, docs) can
/// reference the exact string without duplicating the literal.
pub const OUTLINE_TOOL_PREFIX: &str = "outline_";

/// Where outline tool calls actually execute.
enum Backend {
    /// In-process `OutlineMcpServer` over a local shelf dir (default).
    Embed(OutlineMcpServer),
    /// Remote `outline-mcp --mcp-http` daemon (the SSOT host).
    Remote(RemoteBackend),
}

/// Lazily-connected streamable-HTTP client to the SSOT daemon.
///
/// Construction performs no I/O — the connection is established on the
/// first outline tool call and reused afterwards. This keeps
/// `session_start` synchronous and lock-friendly (its documented "no
/// `.await`, no upstream I/O" invariant), and means a daemon that is down
/// fails loudly *at call time* instead of silently degrading to a local
/// shelf (two writable copies is the failure mode this whole mode exists
/// to remove — never fall back).
struct RemoteBackend {
    /// Endpoint URL, e.g. `http://ssot-host:8486/mcp`.
    url: String,
    /// Bearer token (raw value; the transport adds the `Bearer ` prefix).
    token: Option<String>,
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
                        format!("outline remote connect failed ({}): {e}", self.url),
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
    McpError::internal_error(format!("outline remote call failed ({url}): {e}"), None)
}

/// Outline module — either a thin wrapper around
/// `outline-mcp-rmcp::OutlineMcpServer` (embed mode) or a streamable-HTTP
/// client to a central `outline-mcp --mcp-http` daemon (remote mode).
///
/// Both modes expose the same forwarding helpers that plug into lds's
/// `ServerHandler` composition points (`list_tools`, `call_tool`,
/// `list_resources`, `read_resource`); the `outline_` prefix rewrite is
/// transport-independent.
pub struct OutlineModule {
    backend: Backend,
}

impl OutlineModule {
    /// Construct an embed-mode module rooted at `shelf_dir`. Creates the
    /// directory (and any missing parents) if it does not exist so the
    /// first `select_book` / `init` call succeeds.
    pub fn new(shelf_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&shelf_dir).with_context(|| {
            format!(
                "failed to create outline shelf dir: {}",
                shelf_dir.display()
            )
        })?;
        Ok(Self {
            backend: Backend::Embed(OutlineMcpServer::new(shelf_dir)),
        })
    }

    /// Construct a remote-mode module targeting `url` (a streamable-HTTP
    /// MCP endpoint, e.g. `http://ssot-host:8486/mcp`).
    ///
    /// No I/O happens here — see [`RemoteBackend`] for the lazy-connect
    /// rationale. `token` is the raw bearer token (usually read from the
    /// env var named by `[remote.outline] token_env`).
    pub fn new_remote(url: String, token: Option<String>) -> Self {
        Self {
            backend: Backend::Remote(RemoteBackend {
                url,
                token,
                conn: OnceCell::new(),
            }),
        }
    }

    /// Underlying upstream server (mostly for tests and introspection).
    /// `None` in remote mode — there is no in-process server to inspect.
    pub fn server(&self) -> Option<&OutlineMcpServer> {
        match &self.backend {
            Backend::Embed(s) => Some(s),
            Backend::Remote(_) => None,
        }
    }

    /// List the outline tools with `outline_` prepended to each name.
    ///
    /// Consumers merge the result into their own `list_tools` response.
    pub async fn list_tools_prefixed(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<Vec<Tool>, McpError> {
        let listed = match &self.backend {
            Backend::Embed(server) => ServerHandler::list_tools(server, request, context).await?,
            Backend::Remote(remote) => remote
                .peer()
                .await?
                .list_tools(request)
                .await
                .map_err(|e| remote_err(&remote.url, e))?,
        };
        Ok(listed
            .tools
            .into_iter()
            .map(|mut t| {
                let mut prefixed = String::with_capacity(OUTLINE_TOOL_PREFIX.len() + t.name.len());
                prefixed.push_str(OUTLINE_TOOL_PREFIX);
                prefixed.push_str(&t.name);
                t.name = prefixed.into();
                t
            })
            .collect())
    }

    /// If `request.name` begins with `outline_`, strip the prefix and
    /// forward to the backend. Otherwise return `Ok(None)` so the caller
    /// can try the next dispatch layer.
    pub async fn try_call(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<Option<CallToolResult>, McpError> {
        let Some(stripped) = request.name.strip_prefix(OUTLINE_TOOL_PREFIX) else {
            return Ok(None);
        };
        let stripped_name: String = stripped.to_string();
        let mut inner = request;
        inner.name = stripped_name.into();
        let result = match &self.backend {
            Backend::Embed(server) => ServerHandler::call_tool(server, inner, context).await?,
            Backend::Remote(remote) => remote
                .peer()
                .await?
                .call_tool(inner)
                .await
                .map_err(|e| remote_err(&remote.url, e))?,
        };
        Ok(Some(result))
    }

    /// Forward `list_resources` verbatim. Outline resources live under the
    /// `outline://` URI scheme, so no rewriting is needed.
    pub async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        match &self.backend {
            Backend::Embed(server) => {
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

    /// If `request.uri` begins with `outline://`, forward to the backend.
    /// Otherwise return `Ok(None)`.
    pub async fn try_read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<Option<ReadResourceResult>, McpError> {
        if !request.uri.starts_with("outline://") {
            return Ok(None);
        }
        let result = match &self.backend {
            Backend::Embed(server) => {
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
