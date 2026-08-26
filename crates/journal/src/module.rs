use std::path::PathBuf;

use anyhow::{Context, Result};
use journal_mcp_rmcp::{JournalMcpServer, RunConfig};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResult, Content, ListResourcesResult,
        PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResult, Tool,
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

/// Tools that accept a per-call `source` override (`"local"` | `"remote"`).
///
/// Scope is deliberately the events-native migration pair only: which
/// backend a session talks to is decided by config (`[remote.journal]`
/// present → remote, absent → embed), and that default is what every call
/// uses when `source` is omitted. Export/import are the two calls that
/// *bridge* the copies — with the override, a local→remote migration is
/// `journal_export_events(source="local")` → `journal_import_events()` in
/// one process, zero restarts.
pub const SOURCE_OVERRIDE_TOOLS: [&str; 2] = ["journal_export_events", "journal_import_events"];

/// Parsed value of the per-call `source` argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceOverride {
    /// Session-local EventLog (`{session_root}/workspace/.journal.db`).
    Local,
    /// Configured `[remote.journal]` daemon.
    Remote,
}

/// Extract (and strip) the `source` argument from a journal tool call.
///
/// - Tools outside [`SOURCE_OVERRIDE_TOOLS`] must not carry `source` — a
///   stray value errors loudly instead of being silently dropped (silently
///   dropping it would look like the override was honored).
/// - The key is removed from the arguments in every case: neither backend's
///   upstream schema knows it.
/// - An explicit `null` is treated as omitted (schema advertises the
///   property as nullable).
fn take_source_arg(
    name: &str,
    arguments: &mut Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<Option<SourceOverride>, McpError> {
    let Some(args) = arguments.as_mut() else {
        return Ok(None);
    };
    let Some(value) = args.remove("source") else {
        return Ok(None);
    };
    if !SOURCE_OVERRIDE_TOOLS.contains(&name) {
        return Err(McpError::invalid_params(
            format!(
                "source is only supported on {} / {} (got it on {name})",
                SOURCE_OVERRIDE_TOOLS[0], SOURCE_OVERRIDE_TOOLS[1]
            ),
            None,
        ));
    }
    match &value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::String(s) if s == "local" => Ok(Some(SourceOverride::Local)),
        serde_json::Value::String(s) if s == "remote" => Ok(Some(SourceOverride::Remote)),
        other => Err(McpError::invalid_params(
            format!("invalid source {other}: expected \"local\" or \"remote\""),
            None,
        )),
    }
}

/// Advertise the `source` override on the two migration tools.
///
/// The upstream (journal-mcp) schemas don't know the parameter — it is an
/// lds routing concern — so the listed schemas are patched here to keep the
/// wire contract discoverable.
fn augment_source_schema(tools: &mut [Tool]) {
    for tool in tools.iter_mut() {
        if !SOURCE_OVERRIDE_TOOLS.contains(&tool.name.as_ref()) {
            continue;
        }
        let schema = std::sync::Arc::make_mut(&mut tool.input_schema);
        if let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut()) {
            props.insert(
                "source".to_string(),
                serde_json::json!({
                    "type": ["string", "null"],
                    "description": "Per-call backend override: \"local\" (session-local \
                                    EventLog) or \"remote\" (the configured [remote.journal] \
                                    daemon). Omitted: the backend the config selected. Use \
                                    source=\"local\" on journal_export_events in remote mode \
                                    to read the pre-migration local history.",
                }),
            );
        }
    }
}

/// The `journal_info` tool name — intercepted in [`JournalModule::try_call`]
/// to produce the layered (`mode` / `session` / `config` / `server`)
/// diagnostic instead of the raw upstream payload. Rationale: the upstream
/// tool takes no parameters and reports the daemon's **startup default
/// store**, which in remote all-projects mode reads as "every call lands in
/// default" even though per-call `project_root` injection targets this
/// session's own namespace.
const INFO_TOOL: &str = "journal_info";

/// Assemble the layered `journal_info` payload for remote mode.
///
/// - `session` — where THIS lds session's calls actually land.
/// - `config`  — provenance of the `[remote.journal]` resolution (labels
///   only, never token values), same spirit as `session_info`'s recipe
///   `resolve_chain`.
/// - `server`  — the daemon payload verbatim under `info` (its
///   `project_root` etc. describe the daemon's startup default store, not
///   this session's namespace — the nesting is what disambiguates; the
///   fields themselves are never rewritten).
#[allow(clippy::too_many_arguments)]
fn remote_info_json(
    session_root: &std::path::Path,
    mount: &str,
    project_key: Option<&str>,
    chapters: serde_json::Value,
    url: &str,
    url_source: &str,
    token_source: &str,
    project_key_source: &str,
    daemon_info_text: &str,
) -> String {
    serde_json::json!({
        "mode": "remote",
        "session": {
            "session_root": session_root.display().to_string(),
            "mount": mount,
            "project_key": project_key,
            "chapters": chapters,
        },
        "config": {
            "url_source": url_source,
            "token_source": token_source,
            "project_key_source": project_key_source,
        },
        "server": {
            "url": url,
            "info": parse_or_raw(daemon_info_text),
        },
    })
    .to_string()
}

/// Assemble the layered `journal_info` payload for embed mode. The embed
/// server's own info **is** this session's store state, so it passes through
/// under `server.info`; `config` only records that no remote is configured.
fn embed_info_json(session_root: &std::path::Path, info_text: &str) -> String {
    serde_json::json!({
        "mode": "embed",
        "session": {
            "session_root": session_root.display().to_string(),
            "mount": "session",
        },
        "config": {
            "note": "[remote.journal] not configured (embed mode)",
        },
        "server": {
            "info": parse_or_raw(info_text),
        },
    })
    .to_string()
}

/// Parse `text` as JSON, falling back to the raw string (never lose the
/// upstream payload just because it failed to parse).
fn parse_or_raw(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|_| serde_json::Value::String(text.to_string()))
}

/// First text block of a tool result, if any.
fn first_text(result: &CallToolResult) -> Option<String> {
    result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
}

/// Configuration for a remote-mode [`JournalModule`] — the resolved
/// `[remote.journal]` endpoint plus the provenance labels the layered
/// `journal_info` reports. Built by the caller (lds `main.rs`
/// `resolve_journal_remote`), which is the only place that knows which
/// config layer (env / project config / user config / auto-derivation)
/// produced each value.
pub struct RemoteJournalConfig {
    /// Streamable-HTTP MCP endpoint, e.g. `https://my-journal.fly.dev/mcp`.
    pub url: String,
    /// Where `url` came from (e.g. `"user config.toml [remote.journal] url"`).
    pub url_source: String,
    /// Raw bearer token; the transport adds the `Bearer ` prefix.
    pub token: Option<String>,
    /// Where the token came from — a label, never the value
    /// (e.g. `"default token_file ~/.config/lds/journal-mcp-token"`).
    pub token_source: String,
    /// Remote project root injected into calls that omit `project_root`.
    pub project_key: Option<String>,
    /// Where `project_key` came from (e.g. `"auto (/data/journal/<session-root-basename>)"`).
    pub project_key_source: String,
    /// Session-local root for `source="local"` calls on the migration pair.
    pub local_root: PathBuf,
    /// Which mount produced this module: `"session"` (session_start) or
    /// `"eager-startup-cwd"` (server-startup eager remote mount).
    pub mount: String,
}

/// Where journal tool calls actually execute.
enum Backend {
    /// In-process `JournalMcpServer` over the session-local EventLog
    /// (`{session_root}/workspace/.journal.db`, default).
    Embed {
        server: JournalMcpServer,
        file_projection_path: Option<PathBuf>,
        project_root: PathBuf,
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
    /// Provenance labels for the layered `journal_info` (`config` section).
    url_source: String,
    token_source: String,
    project_key_source: String,
    /// Which mount produced this module (`"session"` / `"eager-startup-cwd"`).
    mount: String,
    /// Session-local root for `source="local"` calls on the migration pair.
    local_root: PathBuf,
    /// Lazily-built embed server over the session-local EventLog. Built on
    /// the first `source="local"` call — never eagerly, because
    /// constructing `JournalMcpServer` creates
    /// `{local_root}/workspace/.journal.db`, and the eager (startup-cwd)
    /// remote mount must not scatter DB files under whatever cwd the host
    /// launched lds in. Boxed to keep `RemoteBackend` (and the `Backend`
    /// enum) slim — the server is a rarely-populated cold path.
    embed: OnceCell<Box<JournalMcpServer>>,
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

    /// Embed server over the session-local EventLog, built on first use
    /// (`source="local"` routing). No file projection is attached —
    /// EventLog-only is all the migration pair needs.
    async fn local_server(&self) -> Result<&JournalMcpServer, McpError> {
        self.embed
            .get_or_try_init(|| async {
                JournalMcpServer::new(RunConfig {
                    project_root: self.local_root.clone(),
                    file_projection: None,
                })
                .map(Box::new)
                .map_err(|e| {
                    McpError::internal_error(
                        format!(
                            "journal local embed init failed ({}): {e}",
                            self.local_root.display()
                        ),
                        None,
                    )
                })
            })
            .await
            .map(|b| &**b)
    }

    /// Best-effort chapter count of this session's remote namespace (the
    /// `session.chapters` field of the layered `journal_info`). Never fails
    /// the info call — errors degrade to a descriptive string so the
    /// diagnostic stays readable when the daemon or namespace is unhealthy.
    async fn session_chapter_count(&self) -> serde_json::Value {
        let Some(key) = &self.project_key else {
            return serde_json::Value::Null;
        };
        let mut args = serde_json::Map::new();
        args.insert(
            "project_root".to_string(),
            serde_json::Value::String(key.clone()),
        );
        let request = CallToolRequestParams::new("journal_chapter_list").with_arguments(args);
        let outcome = async {
            let result = self
                .peer()
                .await?
                .call_tool(request)
                .await
                .map_err(|e| remote_err(&self.url, e))?;
            Ok::<_, McpError>(first_text(&result))
        }
        .await;
        match outcome {
            Ok(Some(text)) => match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(serde_json::Value::Array(rows)) => serde_json::Value::from(rows.len()),
                _ => serde_json::Value::String("chapter_list returned non-array payload".into()),
            },
            Ok(None) => serde_json::Value::String("chapter_list returned no text content".into()),
            Err(e) => serde_json::Value::String(format!("chapter_list failed: {e}")),
        }
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
            project_root: project_root.clone(),
            file_projection: file_projection.clone(),
        };
        let server = JournalMcpServer::new(cfg).context("failed to construct JournalMcpServer")?;
        Ok(Self {
            backend: Backend::Embed {
                server,
                file_projection_path: file_projection,
                project_root,
            },
        })
    }

    /// Construct a remote-mode module targeting `url` (a streamable-HTTP
    /// MCP endpoint, e.g. `https://my-journal.fly.dev/mcp`).
    ///
    /// No I/O happens here — see [`RemoteBackend`] for the lazy-connect
    /// rationale. See [`RemoteJournalConfig`] for the field contracts
    /// (endpoint + provenance labels + session-local root for the
    /// `source="local"` migration-pair override).
    pub fn new_remote(cfg: RemoteJournalConfig) -> Self {
        Self {
            backend: Backend::Remote(RemoteBackend {
                url: cfg.url,
                token: cfg.token,
                project_key: cfg.project_key,
                conn: OnceCell::new(),
                url_source: cfg.url_source,
                token_source: cfg.token_source,
                project_key_source: cfg.project_key_source,
                mount: cfg.mount,
                local_root: cfg.local_root,
                embed: OnceCell::new(),
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
        let mut tools = listed.tools;
        augment_source_schema(&mut tools);
        Ok(tools)
    }

    /// If `request.name` begins with `journal_`, forward to the backend.
    /// Otherwise return `Ok(None)` so the caller can try the next dispatch
    /// layer.
    ///
    /// The migration pair ([`SOURCE_OVERRIDE_TOOLS`]) accepts a per-call
    /// `source: "local" | "remote"` override; omitted, every tool goes to
    /// the config-selected backend. Routing matrix:
    ///
    /// | config mode | omitted        | `source="local"` | `source="remote"` |
    /// |-------------|----------------|------------------|-------------------|
    /// | embed       | embed          | embed            | error (no remote) |
    /// | remote      | remote         | lazy local embed | remote            |
    ///
    /// In remote-routed calls, a configured `project_key` is injected as
    /// `project_root` when the call omits it (`journal_info` excluded — it
    /// takes no parameters and reports daemon-level state). Local-routed
    /// calls get no injection (the daemon-side key is meaningless for the
    /// session-local EventLog); an explicitly passed `project_root` wins on
    /// both sides, unchanged.
    pub async fn try_call(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<Option<CallToolResult>, McpError> {
        if !request.name.starts_with(JOURNAL_TOOL_PREFIX) {
            return Ok(None);
        }
        let mut request = request;
        let source = take_source_arg(request.name.as_ref(), &mut request.arguments)?;
        let is_info = request.name.as_ref() == INFO_TOOL;
        let result = match (&self.backend, source) {
            (
                Backend::Embed {
                    server,
                    project_root,
                    ..
                },
                None | Some(SourceOverride::Local),
            ) => {
                let result = ServerHandler::call_tool(server, request, context).await?;
                match first_text(&result).filter(|_| is_info) {
                    Some(text) => CallToolResult::success(vec![Content::text(embed_info_json(
                        project_root,
                        &text,
                    ))]),
                    None => result,
                }
            }
            (Backend::Embed { .. }, Some(SourceOverride::Remote)) => {
                return Err(McpError::invalid_params(
                    "source=\"remote\" requested but no [remote.journal] endpoint is configured",
                    None,
                ));
            }
            (Backend::Remote(remote), Some(SourceOverride::Local)) => {
                let server = remote.local_server().await?;
                ServerHandler::call_tool(server, request, context).await?
            }
            (Backend::Remote(remote), None | Some(SourceOverride::Remote)) => {
                if let Some(key) = &remote.project_key
                    && !is_info
                {
                    let args = request.arguments.get_or_insert_with(Default::default);
                    if !args.contains_key("project_root") {
                        args.insert(
                            "project_root".to_string(),
                            serde_json::Value::String(key.clone()),
                        );
                    }
                }
                let result = remote
                    .peer()
                    .await?
                    .call_tool(request)
                    .await
                    .map_err(|e| remote_err(&remote.url, e))?;
                match first_text(&result).filter(|_| is_info) {
                    Some(text) => {
                        let chapters = remote.session_chapter_count().await;
                        CallToolResult::success(vec![Content::text(remote_info_json(
                            &remote.local_root,
                            &remote.mount,
                            remote.project_key.as_deref(),
                            chapters,
                            &remote.url,
                            &remote.url_source,
                            &remote.token_source,
                            &remote.project_key_source,
                            &text,
                        ))])
                    }
                    None => result,
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args_of(v: serde_json::Value) -> Option<serde_json::Map<String, serde_json::Value>> {
        Some(v.as_object().expect("test args must be an object").clone())
    }

    #[test]
    fn take_source_omitted_is_none() {
        let mut none = None;
        assert_eq!(
            take_source_arg("journal_export_events", &mut none).unwrap(),
            None
        );
        let mut args = args_of(serde_json::json!({"project_root": "/p"}));
        assert_eq!(
            take_source_arg("journal_export_events", &mut args).unwrap(),
            None
        );
        assert!(args.as_ref().unwrap().contains_key("project_root"));
    }

    #[test]
    fn take_source_parses_and_strips() {
        let mut args = args_of(serde_json::json!({"source": "local", "content": "x"}));
        assert_eq!(
            take_source_arg("journal_import_events", &mut args).unwrap(),
            Some(SourceOverride::Local)
        );
        let map = args.as_ref().unwrap();
        assert!(!map.contains_key("source"), "source must be stripped");
        assert!(map.contains_key("content"));

        let mut args = args_of(serde_json::json!({"source": "remote"}));
        assert_eq!(
            take_source_arg("journal_export_events", &mut args).unwrap(),
            Some(SourceOverride::Remote)
        );

        // Explicit null is treated as omitted (nullable in the schema).
        let mut args = args_of(serde_json::json!({"source": null}));
        assert_eq!(
            take_source_arg("journal_export_events", &mut args).unwrap(),
            None
        );
        assert!(!args.as_ref().unwrap().contains_key("source"));
    }

    #[test]
    fn take_source_rejects_invalid_value() {
        let mut args = args_of(serde_json::json!({"source": "both"}));
        let err = take_source_arg("journal_export_events", &mut args).unwrap_err();
        assert!(err.to_string().contains("invalid source"), "got: {err}");
    }

    #[test]
    fn take_source_rejects_non_override_tools() {
        for name in ["journal_open_chapter", "journal_tail", "journal_dump"] {
            let mut args = args_of(serde_json::json!({"source": "local"}));
            let err = take_source_arg(name, &mut args).unwrap_err();
            assert!(err.to_string().contains("only supported"), "{name}: {err}");
        }
    }

    #[test]
    fn remote_info_json_layers_session_config_server() {
        let daemon = r#"{"project_root":"/data/journal/default","version":"0.8.0"}"#;
        let out = remote_info_json(
            std::path::Path::new("/home/u/proj"),
            "session",
            Some("/data/journal/proj"),
            serde_json::Value::from(48),
            "https://j.example/mcp",
            "user config.toml [remote.journal] url",
            "default token_file ~/.config/lds/journal-mcp-token",
            "auto (/data/journal/<session-root-basename>)",
            daemon,
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["mode"], "remote");
        assert_eq!(v["session"]["session_root"], "/home/u/proj");
        assert_eq!(v["session"]["mount"], "session");
        assert_eq!(v["session"]["project_key"], "/data/journal/proj");
        assert_eq!(v["session"]["chapters"], 48);
        assert_eq!(
            v["config"]["url_source"],
            "user config.toml [remote.journal] url"
        );
        assert!(
            v["config"]["token_source"]
                .as_str()
                .unwrap()
                .contains("token_file")
        );
        // daemon payload passes through verbatim under server.info
        assert_eq!(v["server"]["url"], "https://j.example/mcp");
        assert_eq!(v["server"]["info"]["project_root"], "/data/journal/default");
        assert_eq!(v["server"]["info"]["version"], "0.8.0");
    }

    #[test]
    fn remote_info_json_survives_unparseable_daemon_payload() {
        let out = remote_info_json(
            std::path::Path::new("/p"),
            "eager-startup-cwd",
            None,
            serde_json::Value::Null,
            "https://j.example/mcp",
            "env LDS_JOURNAL_REMOTE_URL",
            "none",
            "env LDS_JOURNAL_REMOTE_PROJECT_KEY",
            "not json at all",
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["server"]["info"], "not json at all");
        assert_eq!(v["session"]["project_key"], serde_json::Value::Null);
    }

    #[test]
    fn embed_info_json_wraps_passthrough() {
        let out = embed_info_json(
            std::path::Path::new("/home/u/proj"),
            r#"{"project_root":"/home/u/proj","db_exists":true}"#,
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["mode"], "embed");
        assert_eq!(v["session"]["session_root"], "/home/u/proj");
        assert_eq!(v["session"]["mount"], "session");
        assert_eq!(v["server"]["info"]["db_exists"], true);
        assert!(
            v["config"]["note"]
                .as_str()
                .unwrap()
                .contains("not configured")
        );
    }

    #[test]
    fn augment_adds_source_to_migration_pair_only() {
        let mut tools: Vec<Tool> = serde_json::from_value(serde_json::json!([
            {
                "name": "journal_export_events",
                "description": "d",
                "inputSchema": {"type": "object", "properties": {"project_root": {"type": ["string", "null"]}}}
            },
            {
                "name": "journal_tail",
                "description": "d",
                "inputSchema": {"type": "object", "properties": {"n": {"type": ["integer", "null"]}}}
            }
        ]))
        .expect("tool fixtures deserialize");
        augment_source_schema(&mut tools);
        let export_props = tools[0].input_schema["properties"].as_object().unwrap();
        assert!(
            export_props.contains_key("source"),
            "export must advertise source"
        );
        let tail_props = tools[1].input_schema["properties"].as_object().unwrap();
        assert!(
            !tail_props.contains_key("source"),
            "tail must not advertise source"
        );
    }
}
