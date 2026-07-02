use std::path::PathBuf;

use anyhow::{Context, Result};
use journal_mcp_rmcp::{JournalMcpServer, RunConfig};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResult, ListResourcesResult, PaginatedRequestParams,
        ReadResourceRequestParams, ReadResourceResult, Tool,
    },
    service::RequestContext,
};

/// Prefix that every journal-mcp tool name carries in-crate.
///
/// Used by [`JournalModule::try_call`] to decide whether an incoming lds
/// tool call should be forwarded to `JournalMcpServer`. Kept public so
/// downstream code (routing tables, tests, docs) can reference the exact
/// string without duplicating the literal.
pub const JOURNAL_TOOL_PREFIX: &str = "journal_";

/// Journal module — thin wrapper around `journal-mcp-rmcp::JournalMcpServer`.
///
/// Holds the upstream server (which internally owns the `JournalCore`,
/// per-project cache, and startup-time `FileProjection` attachment) and
/// exposes forwarding helpers that plug into lds's `ServerHandler`
/// composition points (`list_tools`, `call_tool`, `list_resources`,
/// `read_resource`).
pub struct JournalModule {
    server: JournalMcpServer,
    file_projection_path: Option<PathBuf>,
}

impl JournalModule {
    /// Construct a new module rooted at `project_root` (typically
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
            server,
            file_projection_path: file_projection,
        })
    }

    /// Underlying upstream server (mostly for tests and introspection).
    pub fn server(&self) -> &JournalMcpServer {
        &self.server
    }

    /// Path of the startup-attached FileProjection, if any. Runtime
    /// attach/detach via `journal_projection_attach` /
    /// `journal_projection_detach` is not reflected here — this only
    /// reports the constructor-time value (consistent with the previous
    /// `JournalModule::file_projection_path` behaviour).
    pub fn file_projection_path(&self) -> Option<PathBuf> {
        self.file_projection_path.clone()
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
        let listed = ServerHandler::list_tools(&self.server, request, context).await?;
        Ok(listed.tools)
    }

    /// If `request.name` begins with `journal_`, forward to the upstream
    /// server verbatim. Otherwise return `Ok(None)` so the caller can try
    /// the next dispatch layer.
    pub async fn try_call(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<Option<CallToolResult>, McpError> {
        if !request.name.starts_with(JOURNAL_TOOL_PREFIX) {
            return Ok(None);
        }
        let result = ServerHandler::call_tool(&self.server, request, context).await?;
        Ok(Some(result))
    }

    /// Forward `list_resources` verbatim.
    pub async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        ServerHandler::list_resources(&self.server, request, context).await
    }

    /// If `request.uri` begins with `journal-mcp://`, forward to the
    /// upstream server. Otherwise return `Ok(None)`.
    pub async fn try_read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<Option<ReadResourceResult>, McpError> {
        if !request.uri.starts_with("journal-mcp://") {
            return Ok(None);
        }
        let result = ServerHandler::read_resource(&self.server, request, context).await?;
        Ok(Some(result))
    }
}
