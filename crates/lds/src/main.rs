use std::sync::Arc;

use anyhow::Result;
use lds_core::{LdsState, SessionConfig};
use lds_git::GitModule;
use lds_recipe::RecipeModule;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
use schemars::JsonSchema;
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt};
use serde::Deserialize;
use tokio::sync::RwLock;

#[derive(Clone)]
struct LdsServer {
    state: Arc<RwLock<Inner>>,
}

struct Inner {
    lds: LdsState,
    git: Option<GitModule>,
    recipe: Option<RecipeModule>,
}

impl LdsServer {
    fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(Inner {
                lds: LdsState::new(),
                git: None,
                recipe: None,
            })),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SessionStartReq {
    root: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    max_output: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GitLogReq {
    #[serde(default = "default_max_count")]
    max_count: usize,
}

fn default_max_count() -> usize {
    20
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RecipeRunReq {
    recipe: String,
    #[serde(default)]
    args: Vec<String>,
}

#[tool_router]
impl LdsServer {
    #[tool(description = "Initialize session with project root. Must be called first.")]
    async fn session_start(
        &self,
        Parameters(req): Parameters<SessionStartReq>,
    ) -> Result<CallToolResult, McpError> {
        let mut inner = self.state.write().await;
        let config = SessionConfig {
            root: req.root.into(),
            timeout_secs: req.timeout_secs,
            max_output: req.max_output,
        };
        let session = inner
            .lds
            .start_session(config)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        inner.git = Some(GitModule::new(Arc::clone(&session)));
        inner.recipe = Some(RecipeModule::new(Arc::clone(&session)));
        Ok(CallToolResult::success(vec![Content::text(format!(
            "session started: root={}, id={}",
            session.root().display(),
            session.id()
        ))]))
    }

    #[tool(description = "Show git working tree status")]
    async fn git_status(&self) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner
            .git
            .as_ref()
            .ok_or_else(|| McpError::internal_error("no session", None))?;
        let out = git
            .status()
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(description = "Show git commit log")]
    async fn git_log(
        &self,
        Parameters(req): Parameters<GitLogReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner
            .git
            .as_ref()
            .ok_or_else(|| McpError::internal_error("no session", None))?;
        let out = git
            .log(req.max_count)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(description = "Show git diff (working tree vs HEAD)")]
    async fn git_diff(&self) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner
            .git
            .as_ref()
            .ok_or_else(|| McpError::internal_error("no session", None))?;
        let out = git
            .diff()
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(description = "List available justfile recipes")]
    async fn recipe_list(&self) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let recipe = inner
            .recipe
            .as_ref()
            .ok_or_else(|| McpError::internal_error("no session", None))?;
        let recipes = recipe
            .list()
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let json = serde_json::to_string_pretty(&recipes)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(description = "Run a justfile recipe")]
    async fn recipe_run(
        &self,
        Parameters(req): Parameters<RecipeRunReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let recipe = inner
            .recipe
            .as_ref()
            .ok_or_else(|| McpError::internal_error("no session", None))?;
        let args_refs: Vec<&str> = req.args.iter().map(|s| s.as_str()).collect();
        let output = recipe
            .run(&req.recipe, &args_refs)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let json = serde_json::to_string_pretty(&output)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }
}

#[tool_handler]
impl ServerHandler for LdsServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.instructions = Some("local-develop-server: unified MCP for orch pipeline".into());
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    tracing::info!("lds v{}", env!("CARGO_PKG_VERSION"));

    let server = LdsServer::new();
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
