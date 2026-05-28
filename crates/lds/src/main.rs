use std::collections::HashMap;
use std::path::PathBuf;
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
    #[serde(default)]
    global_recipe_dir: Option<String>,
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
struct GitCommitReq {
    working_dir: String,
    message: String,
    #[serde(default)]
    paths: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GitWorktreeAddReq {
    name: String,
    branch: String,
    #[serde(default)]
    base_branch: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GitWorktreeRemoveReq {
    name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GitMergeReq {
    branch: String,
    into_branch: String,
    #[serde(default)]
    working_dir: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GitBranchDeleteReq {
    branch: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RecipeRunReq {
    recipe: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    content: HashMap<String, String>,
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
            global_recipe_dir: req.global_recipe_dir.map(Into::into),
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

    #[tool(description = "List git worktrees with session ownership annotation")]
    async fn git_worktree_list(&self) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner
            .git
            .as_ref()
            .ok_or_else(|| McpError::internal_error("no session", None))?;
        let out = git
            .worktree_list()
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(description = "Create a git worktree under .worktrees/ with a new branch")]
    async fn git_worktree_add(
        &self,
        Parameters(req): Parameters<GitWorktreeAddReq>,
    ) -> Result<CallToolResult, McpError> {
        let mut inner = self.state.write().await;
        let git = inner
            .git
            .as_mut()
            .ok_or_else(|| McpError::internal_error("no session", None))?;
        let out = git
            .worktree_add(&req.name, &req.branch, req.base_branch.as_deref())
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(description = "Remove a session-owned git worktree")]
    async fn git_worktree_remove(
        &self,
        Parameters(req): Parameters<GitWorktreeRemoveReq>,
    ) -> Result<CallToolResult, McpError> {
        let mut inner = self.state.write().await;
        let git = inner
            .git
            .as_mut()
            .ok_or_else(|| McpError::internal_error("no session", None))?;
        let out = git
            .worktree_remove(&req.name)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(description = "Stage and commit changes in a session-owned working directory")]
    async fn git_commit(
        &self,
        Parameters(req): Parameters<GitCommitReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner
            .git
            .as_ref()
            .ok_or_else(|| McpError::internal_error("no session", None))?;
        let working_dir = PathBuf::from(&req.working_dir);
        let out = git
            .commit(&working_dir, &req.message, req.paths.as_deref())
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(description = "Merge a branch into another in a session-owned working directory")]
    async fn git_merge(
        &self,
        Parameters(req): Parameters<GitMergeReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner
            .git
            .as_ref()
            .ok_or_else(|| McpError::internal_error("no session", None))?;
        let session = inner
            .lds
            .session()
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let working_dir = req
            .working_dir
            .map(PathBuf::from)
            .unwrap_or_else(|| session.root().to_path_buf());
        let out = git
            .merge(&req.branch, &req.into_branch, &working_dir)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(description = "Delete a session-owned branch")]
    async fn git_branch_delete(
        &self,
        Parameters(req): Parameters<GitBranchDeleteReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner
            .git
            .as_ref()
            .ok_or_else(|| McpError::internal_error("no session", None))?;
        let out = git
            .branch_delete(&req.branch)
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
            .run(&req.recipe, &args_refs, &req.content, None)
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
