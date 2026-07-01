//! Test-only mock upstream MCP server for `lds-router` export integration
//! tests (`tests/export_test.rs`).
//!
//! Speaks MCP over stdio like any other upstream `RouteClient` subprocess,
//! but its advertised tool list is re-read from a `--tools-file <path>` JSON
//! document on *every* `list_tools` call rather than cached at startup — so
//! a test can simulate upstream schema drift by rewriting the file between
//! two `ExportRegistry::refresh` calls against the same already-spawned
//! subprocess.
//!
//! `--tools-file` content shape: a JSON array of
//! `{"name": "...", "description": "...", "input_schema": {...}}` objects.
//! Malformed or missing entries are skipped rather than causing the process
//! to exit, so tests can also exercise "some declared tools go missing"
//! scenarios by simply shrinking the array.

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, ServiceExt};

#[derive(Clone)]
struct MockUpstream {
    tools_file: Arc<PathBuf>,
}

impl MockUpstream {
    fn read_tools(&self) -> Vec<Tool> {
        let content =
            std::fs::read_to_string(self.tools_file.as_ref()).unwrap_or_else(|_| "[]".to_string());
        let entries: Vec<serde_json::Value> = serde_json::from_str(&content).unwrap_or_default();
        entries
            .into_iter()
            .filter_map(|entry| {
                let name = entry.get("name")?.as_str()?.to_string();
                let description = entry
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or_default()
                    .to_string();
                let schema = entry
                    .get("input_schema")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({ "type": "object", "properties": {} }));
                let serde_json::Value::Object(schema_map) = schema else {
                    return None;
                };
                Some(Tool::new(name, description, Arc::new(schema_map)))
            })
            .collect()
    }
}

impl ServerHandler for MockUpstream {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: self.read_tools(),
            meta: None,
            next_cursor: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::success(vec![Content::text(format!(
            "mock-upstream-called:{}",
            request.name
        ))]))
    }
}

fn parse_tools_file_arg() -> Option<PathBuf> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--tools-file" {
            return args.next().map(PathBuf::from);
        }
    }
    None
}

#[tokio::main]
async fn main() {
    let Some(tools_file) = parse_tools_file_arg() else {
        eprintln!("mock_upstream: --tools-file <path> is required");
        std::process::exit(1);
    };
    let server = MockUpstream {
        tools_file: Arc::new(tools_file),
    };
    let service = match server.serve(rmcp::transport::io::stdio()).await {
        Ok(service) => service,
        Err(e) => {
            eprintln!("mock_upstream: failed to serve: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = service.waiting().await {
        eprintln!("mock_upstream: service exited with error: {e}");
        std::process::exit(1);
    }
}
