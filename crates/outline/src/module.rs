use std::path::PathBuf;

use anyhow::{Context, Result};
use outline_mcp_rmcp::OutlineMcpServer;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResult, ListResourcesResult, PaginatedRequestParams,
        ReadResourceRequestParams, ReadResourceResult, Tool,
    },
    service::RequestContext,
};

/// Prefix applied to every outline tool name when surfaced from lds.
///
/// Kept public so downstream code (routing tables, tests, docs) can
/// reference the exact string without duplicating the literal.
pub const OUTLINE_TOOL_PREFIX: &str = "outline_";

/// Outline module — thin wrapper around `outline-mcp-rmcp::OutlineMcpServer`.
///
/// Holds the upstream server (which internally owns the shelf directory and
/// the currently selected book) and exposes forwarding helpers that plug
/// into lds's `ServerHandler` composition points (`list_tools`,
/// `call_tool`, `list_resources`, `read_resource`).
pub struct OutlineModule {
    server: OutlineMcpServer,
}

impl OutlineModule {
    /// Construct a new module rooted at `shelf_dir`. Creates the directory
    /// (and any missing parents) if it does not exist so the first
    /// `select_book` / `init` call succeeds.
    pub fn new(shelf_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&shelf_dir).with_context(|| {
            format!(
                "failed to create outline shelf dir: {}",
                shelf_dir.display()
            )
        })?;
        Ok(Self {
            server: OutlineMcpServer::new(shelf_dir),
        })
    }

    /// Underlying upstream server (mostly for tests and introspection).
    pub fn server(&self) -> &OutlineMcpServer {
        &self.server
    }

    /// List the outline tools with `outline_` prepended to each name.
    ///
    /// Consumers merge the result into their own `list_tools` response.
    pub async fn list_tools_prefixed(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<Vec<Tool>, McpError> {
        let listed = ServerHandler::list_tools(&self.server, request, context).await?;
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
    /// forward to the upstream server. Otherwise return `Ok(None)` so the
    /// caller can try the next dispatch layer.
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
        let result = ServerHandler::call_tool(&self.server, inner, context).await?;
        Ok(Some(result))
    }

    /// Forward `list_resources` verbatim. Outline resources live under the
    /// `outline://` URI scheme, so no rewriting is needed.
    pub async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        ServerHandler::list_resources(&self.server, request, context).await
    }

    /// If `request.uri` begins with `outline://`, forward to the upstream
    /// server. Otherwise return `Ok(None)`.
    pub async fn try_read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<Option<ReadResourceResult>, McpError> {
        if !request.uri.starts_with("outline://") {
            return Ok(None);
        }
        let result = ServerHandler::read_resource(&self.server, request, context).await?;
        Ok(Some(result))
    }
}
