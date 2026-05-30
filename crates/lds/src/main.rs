mod cli;

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use lds_core::config::Config;
use lds_core::{LdsState, Session, SessionConfig, check_binaries};
use lds_git::GitModule;
use lds_recipe::RecipeModule;
use lds_sandbox::fs::SandboxFs;
use lds_sandbox::python::SandboxPython;
use rmcp::RoleServer;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
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
    sandbox_fs: Option<SandboxFs>,
    sandbox_python: Option<SandboxPython>,
    startup_cwd: Option<PathBuf>,
    startup_global_dirs: Arc<Vec<PathBuf>>,
}

/// Merge all configured global recipe directory sources into a single ordered list.
///
/// Resolution priority (crux 1):
///   1. `cfg.paths.global_justfile` — if set, its **directory** is prepended (highest)
///   2. `cfg.recipes.dirs`          — paths from `~/.config/lds/config.toml`
///   3. `env_var`                   — `LDS_RECIPE_GLOBAL_DIRS` colon-separated paths (lowest)
///
/// Project-level justfiles are NOT included here; `build_resolve_chain` appends
/// the project justfile automatically when given the session root.
///
/// The `env_var` parameter is injectable for unit testing without mutating the
/// real environment.
fn resolve_startup_global_dirs(cfg: Config, env_var: Option<OsString>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();

    // (highest) paths.global_justfile — its parent directory becomes a recipe dir.
    if let Some(path) = cfg.paths.global_justfile {
        // The path may still contain a tilde if the user edited config.toml by hand.
        // Expand it defensively before storing.
        let expanded = match lds_core::config::tilde_expand(&path.to_string_lossy()) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("failed to expand global_justfile path: {e}");
                path
            }
        };
        dirs.push(expanded);
    }

    // (next) recipes.dirs from config.toml — already absolute (tilde-expanded by Config::load).
    let config_dirs_count = cfg.recipes.dirs.len();
    dirs.extend(cfg.recipes.dirs);

    // (lowest) LDS_RECIPE_GLOBAL_DIRS env var — colon-separated on Unix.
    let env_dirs: Vec<PathBuf> = env_var
        .map(|v| std::env::split_paths(&v).collect())
        .unwrap_or_default();
    let env_dirs_count = env_dirs.len();
    dirs.extend(env_dirs);

    tracing::info!(
        config_dirs_count,
        env_dirs_count,
        "global recipe dirs resolved"
    );

    dirs
}

impl LdsServer {
    async fn list_plugin_tools(&self) -> Result<Vec<Tool>, McpError> {
        let inner = self.state.read().await;
        let global_dirs = inner.startup_global_dirs.clone();
        let mut plugins = lds_recipe::list_global_plugins(&*global_dirs)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if let Some(recipe) = inner.recipe.as_ref() {
            let project = recipe
                .list_plugins()
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            // Project plugins override global on name collision.
            let mut by_name: std::collections::HashMap<String, lds_recipe::PluginRecipe> =
                plugins.into_iter().map(|p| (p.name.clone(), p)).collect();
            for p in project {
                by_name.insert(p.name.clone(), p);
            }
            plugins = by_name.into_values().collect();
            plugins.sort_by(|a, b| a.name.cmp(&b.name));
        }

        Ok(plugins.into_iter().map(plugin_to_tool).collect())
    }

    async fn try_plugin_call(
        &self,
        name: &str,
        arguments: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Result<Option<CallToolResult>, McpError> {
        let inner = self.state.read().await;
        let Some(recipe) = inner.recipe.as_ref() else {
            // No session yet — cannot dispatch plugins (recipe module needs session for
            // execution). If the requested tool is a static built-in the router can still
            // handle it (e.g. session_start itself), so fall through with Ok(None). If the
            // tool is NOT a built-in it must be a plugin call — return the unified
            // no-session error (-32603) instead of letting it become a tool-not-found (R-W2a).
            let is_builtin = Self::tool_router()
                .list_all()
                .iter()
                .any(|t| t.name.as_ref() == name);
            if is_builtin {
                return Ok(None);
            }
            tracing::warn!(tool = name, "plugin call attempted without active session");
            return Err(no_session_error());
        };

        let global_dirs = inner.startup_global_dirs.clone();
        let mut plugins = lds_recipe::list_global_plugins(&*global_dirs)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        plugins.extend(
            recipe
                .list_plugins()
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?,
        );
        let Some(target) = plugins.iter().find(|p| p.name == name) else {
            return Ok(None);
        };

        // Build positional args from recipe parameters, in declaration order.
        // Skip trailing parameters whose value falls back to the recipe default.
        let arg_strings: Vec<String> = target
            .parameters
            .iter()
            .map(|p| {
                arguments
                    .and_then(|a| a.get(&p.name))
                    .map(|v| match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .unwrap_or_default()
            })
            .collect();
        // Trim trailing empty strings (parameters left to recipe default).
        let last_present = arg_strings
            .iter()
            .rposition(|s| !s.is_empty())
            .map(|i| i + 1)
            .unwrap_or(0);
        let positional: Vec<&str> = arg_strings[..last_present]
            .iter()
            .map(|s| s.as_str())
            .collect();

        let output = recipe
            .run(name, &positional, &HashMap::new(), None)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let json = serde_json::to_string_pretty(&output)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(Some(CallToolResult::success(vec![Content::text(json)])))
    }

    fn new() -> Self {
        let startup_cwd = std::env::current_dir().ok();
        let cfg = Config::load_or_default();
        let env_var = std::env::var_os("LDS_RECIPE_GLOBAL_DIRS");
        let startup_global_dirs = Arc::new(resolve_startup_global_dirs(cfg, env_var));
        Self {
            state: Arc::new(RwLock::new(Inner {
                lds: LdsState::new(),
                git: None,
                recipe: None,
                sandbox_fs: None,
                sandbox_python: None,
                startup_cwd,
                startup_global_dirs,
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
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SandboxWriteReq {
    path: String,
    content: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SandboxEditReq {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SandboxAppendReq {
    path: String,
    content: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SandboxReadReq {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SandboxLinesReq {
    path: String,
    #[serde(default)]
    lines: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SandboxRollbackReq {
    path: String,
    #[serde(default)]
    snapshot_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SandboxHistoryReq {
    path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SandboxPythonReq {
    script: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SandboxPythonFileReq {
    path: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RecipeLogsReq {
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    tail: Option<usize>,
}

/// Shared factory for the "no session active" MCP error.
///
/// All tool handlers that require an active session use this factory so that
/// the error code (-32603) and message ("no session") are defined in one place.
/// Infallible — never fails.
fn no_session_error() -> McpError {
    McpError::internal_error("no session", None)
}

/// Build and initialize all session-dependent modules on `inner`.
///
/// This is the single construction path shared by the explicit `session_start`
/// handler and the auto-start hook in `call_tool`. Inlining the construction
/// logic in two separate places would allow the two paths to diverge, breaking
/// session invariants (crux §1).
fn build_session_modules(
    inner: &mut Inner,
    config: SessionConfig,
) -> Result<Arc<Session>, McpError> {
    let session = inner
        .lds
        .start_session(config)
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    inner.git = Some(GitModule::new(Arc::clone(&session)));
    inner.recipe = Some(RecipeModule::new(Arc::clone(&session)));
    inner.sandbox_fs = Some(
        SandboxFs::new(session.root())
            .map_err(|e| McpError::internal_error(e.to_string(), None))?,
    );
    inner.sandbox_python = Some(SandboxPython::new(session.root()));
    Ok(session)
}

/// Return `true` if `path` is a ProjectRoot: a directory that contains a
/// `.git` entry or a `justfile`. Equivalent to task-mcp's ProjectRoot check.
fn is_project_root(path: &std::path::Path) -> bool {
    path.join(".git").exists() || path.join("justfile").exists()
}

#[tool_router]
impl LdsServer {
    #[tool(description = "Initialize session with project root. Must be called first.")]
    async fn session_start(
        &self,
        Parameters(req): Parameters<SessionStartReq>,
    ) -> Result<CallToolResult, McpError> {
        let mut inner = self.state.write().await;
        // Adapter: compose global_recipe_dirs from the MCP single arg (if any)
        // followed by startup_global_dirs (config.toml dirs then env dirs) in
        // declaration order.
        // Precedence (low→high): default ~/.config/lds → config.toml dirs → env dirs → MCP wire arg → project.
        let startup_dirs = inner.startup_global_dirs.clone();
        let mut global_recipe_dirs: Vec<PathBuf> = req
            .global_recipe_dir
            .map(|s| vec![PathBuf::from(s)])
            .unwrap_or_default();
        global_recipe_dirs.extend(startup_dirs.iter().cloned());
        let config = SessionConfig {
            root: req.root.into(),
            timeout_secs: req.timeout_secs,
            max_output: req.max_output,
            global_recipe_dirs,
        };
        let session = build_session_modules(&mut inner, config)?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "session started: root={}, id={}",
            session.root().display(),
            session.id()
        ))]))
    }

    #[tool(description = "Show git working tree status")]
    async fn git_status(&self) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner.git.as_ref().ok_or_else(no_session_error)?;
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
        let git = inner.git.as_ref().ok_or_else(no_session_error)?;
        let out = git
            .log(req.max_count)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(description = "Show git diff (working tree vs HEAD)")]
    async fn git_diff(&self) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner.git.as_ref().ok_or_else(no_session_error)?;
        let out = git
            .diff()
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(description = "List git worktrees with session ownership annotation")]
    async fn git_worktree_list(&self) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner.git.as_ref().ok_or_else(no_session_error)?;
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
        let git = inner.git.as_mut().ok_or_else(no_session_error)?;
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
        let git = inner.git.as_mut().ok_or_else(no_session_error)?;
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
        let git = inner.git.as_ref().ok_or_else(no_session_error)?;
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
        let git = inner.git.as_ref().ok_or_else(no_session_error)?;
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
        let git = inner.git.as_ref().ok_or_else(no_session_error)?;
        let out = git
            .branch_delete(&req.branch)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(description = "List available justfile recipes")]
    async fn recipe_list(&self) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let recipe = inner.recipe.as_ref().ok_or_else(no_session_error)?;
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
        let recipe = inner.recipe.as_ref().ok_or_else(no_session_error)?;
        let args_refs: Vec<&str> = req.args.iter().map(|s| s.as_str()).collect();
        let output = recipe
            .run(&req.recipe, &args_refs, &req.content, req.timeout_secs)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let json = serde_json::to_string_pretty(&output)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Query recipe execution logs. By ID returns full record; without ID returns recent summaries."
    )]
    async fn recipe_logs(
        &self,
        Parameters(req): Parameters<RecipeLogsReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let recipe = inner.recipe.as_ref().ok_or_else(no_session_error)?;
        let logs = recipe.logs();

        let json = if let Some(ref id) = req.task_id {
            let entry = logs
                .get(id)
                .ok_or_else(|| McpError::internal_error(format!("log not found: {id}"), None))?;
            if let Some(tail) = req.tail {
                let lines: Vec<&str> = entry.stdout.lines().collect();
                let start = lines.len().saturating_sub(tail);
                let mut trimmed = entry.clone();
                trimmed.stdout = lines[start..].join("\n");
                serde_json::to_string_pretty(&trimmed)
            } else {
                serde_json::to_string_pretty(&entry)
            }
        } else {
            let recent = logs.recent(10);
            let summaries: Vec<lds_recipe::RecipeOutputSummary> =
                recent.iter().map(Into::into).collect();
            serde_json::to_string_pretty(&summaries)
        };

        let json = json.map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Sandboxed write: full file content. Auto-snapshots pre-state for rollback."
    )]
    async fn sandbox_write(
        &self,
        Parameters(req): Parameters<SandboxWriteReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let fs = inner.sandbox_fs.as_ref().ok_or_else(no_session_error)?;
        let result = fs
            .write(&req.path, &req.content)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Sandboxed edit: replace old_string with new_string. Auto-snapshots pre-state."
    )]
    async fn sandbox_edit(
        &self,
        Parameters(req): Parameters<SandboxEditReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let fs = inner.sandbox_fs.as_ref().ok_or_else(no_session_error)?;
        let result = fs
            .edit(&req.path, &req.old_string, &req.new_string, req.replace_all)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(description = "Sandboxed append: add content to end of file. Auto-snapshots pre-state.")]
    async fn sandbox_append(
        &self,
        Parameters(req): Parameters<SandboxAppendReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let fs = inner.sandbox_fs.as_ref().ok_or_else(no_session_error)?;
        let result = fs
            .append(&req.path, &req.content)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(description = "Sandboxed read: read file with optional offset (line) and limit.")]
    async fn sandbox_read(
        &self,
        Parameters(req): Parameters<SandboxReadReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let fs = inner.sandbox_fs.as_ref().ok_or_else(no_session_error)?;
        let content = fs
            .read(&req.path, req.offset, req.limit)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(content)]))
    }

    #[tool(description = "Sandboxed head: read first N lines (default 20).")]
    async fn sandbox_head(
        &self,
        Parameters(req): Parameters<SandboxLinesReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let fs = inner.sandbox_fs.as_ref().ok_or_else(no_session_error)?;
        let content = fs
            .head(&req.path, req.lines)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(content)]))
    }

    #[tool(description = "Sandboxed tail: read last N lines (default 20).")]
    async fn sandbox_tail(
        &self,
        Parameters(req): Parameters<SandboxLinesReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let fs = inner.sandbox_fs.as_ref().ok_or_else(no_session_error)?;
        let content = fs
            .tail(&req.path, req.lines)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(content)]))
    }

    #[tool(
        description = "Rollback file to a prior snapshot. Omit snapshot_id to restore the most recent."
    )]
    async fn sandbox_rollback(
        &self,
        Parameters(req): Parameters<SandboxRollbackReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let fs = inner.sandbox_fs.as_ref().ok_or_else(no_session_error)?;
        let result = fs
            .rollback(&req.path, req.snapshot_id.as_deref())
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(description = "List snapshot history for a file (newest first).")]
    async fn sandbox_history(
        &self,
        Parameters(req): Parameters<SandboxHistoryReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let fs = inner.sandbox_fs.as_ref().ok_or_else(no_session_error)?;
        let history = fs
            .history(&req.path)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let json = serde_json::to_string_pretty(&history)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Run Python script in a sandboxed subprocess with 3-layer preamble guard (module deny + import guard + os attr removal)."
    )]
    async fn sandbox_python(
        &self,
        Parameters(req): Parameters<SandboxPythonReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let base = inner
            .sandbox_python
            .as_ref()
            .ok_or_else(no_session_error)?
            .clone();
        let py = match req.timeout_secs {
            Some(secs) => base.with_timeout(secs),
            None => base,
        };
        let result = py
            .execute(&req.script)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Run a Python file in the sandboxed subprocess. Uses the same preamble guard as sandbox_python."
    )]
    async fn sandbox_python_file(
        &self,
        Parameters(req): Parameters<SandboxPythonFileReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let base = inner
            .sandbox_python
            .as_ref()
            .ok_or_else(no_session_error)?
            .clone();
        let py = match req.timeout_secs {
            Some(secs) => base.with_timeout(secs),
            None => base,
        };
        let result = py
            .execute_file(std::path::Path::new(&req.path))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(description = "Show current session state (id, root, mode, justfile paths)")]
    async fn session_info(&self) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let session = inner
            .lds
            .session()
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let resolve_info: Vec<String> = inner
            .recipe
            .as_ref()
            .map(|r| {
                r.resolve_chain()
                    .iter()
                    .map(|(level, path)| format!("{level:?}: {}", path.display()))
                    .collect()
            })
            .unwrap_or_default();

        let binaries = check_binaries(&["git", "just", "python3", "codedash", "rg"]);
        let binary_lines: Vec<String> = binaries
            .iter()
            .map(|b| {
                let status = if b.available { "available" } else { "MISSING" };
                let path = b.path.as_deref().unwrap_or("-");
                format!("  - {}: {status} ({path})", b.name)
            })
            .collect();

        let global_dirs_display = {
            let dirs = session.global_recipe_dirs();
            if dirs.is_empty() {
                "(default)".to_string()
            } else {
                dirs.iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        };
        let info = format!(
            "session_id: {}\nroot: {}\nglobal_recipe_dirs: {}\njustfiles:\n{}\nexternal tools:\n{}",
            session.id(),
            session.root().display(),
            global_dirs_display,
            resolve_info
                .iter()
                .map(|s| format!("  - {s}"))
                .collect::<Vec<_>>()
                .join("\n"),
            binary_lines.join("\n"),
        );
        Ok(CallToolResult::success(vec![Content::text(info)]))
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

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let mut tools = Self::tool_router().list_all();
        let plugins = self.list_plugin_tools().await.unwrap_or_default();
        tools.extend(plugins);
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(ListToolsResult {
            tools,
            meta: None,
            next_cursor: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // Auto-start: if the server was launched from a ProjectRoot and no
        // session has been started yet, start one automatically. Skip when the
        // tool being called is session_start itself (that handler will start
        // the session explicitly). The write guard is kept in a tight scope and
        // dropped before dispatch so that try_plugin_call can acquire a read
        // guard without deadlocking (R3).
        if request.name != "session_start" {
            let mut inner = self.state.write().await;
            if inner.lds.session().is_err()
                && let Some(cwd) = inner.startup_cwd.clone()
                && is_project_root(&cwd)
            {
                // Auto-start: use startup_global_dirs (config.toml + env) so plugins are resolved correctly.
                let global_recipe_dirs = (*inner.startup_global_dirs).clone();
                let config = SessionConfig {
                    root: cwd,
                    timeout_secs: None,
                    max_output: None,
                    global_recipe_dirs,
                };
                build_session_modules(&mut inner, config)?;
            }
            // write guard drops here — before try_plugin_call takes a read guard
        }

        if let Some(result) = self
            .try_plugin_call(&request.name, request.arguments.as_ref())
            .await?
        {
            return Ok(result);
        }
        let tcc = ToolCallContext::new(self, request, context);
        Self::tool_router().call(tcc).await
    }
}

fn plugin_to_tool(plugin: lds_recipe::PluginRecipe) -> Tool {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for p in &plugin.parameters {
        let mut prop = serde_json::Map::new();
        prop.insert(
            "type".to_string(),
            serde_json::Value::String("string".to_string()),
        );
        properties.insert(p.name.clone(), serde_json::Value::Object(prop));
        if p.default.is_none() {
            required.push(serde_json::Value::String(p.name.clone()));
        }
    }
    let mut schema = serde_json::Map::new();
    schema.insert(
        "type".to_string(),
        serde_json::Value::String("object".to_string()),
    );
    schema.insert(
        "properties".to_string(),
        serde_json::Value::Object(properties),
    );
    if !required.is_empty() {
        schema.insert("required".to_string(), serde_json::Value::Array(required));
    }

    let description = if plugin.description.is_empty() {
        format!("Plugin recipe: {}", plugin.name)
    } else {
        plugin.description
    };

    Tool::new(plugin.name, description, Arc::new(schema))
}

/// MCP serve mode: initialise the server and run until the transport closes.
async fn serve_mcp() -> Result<()> {
    tracing::info!("lds v{}", env!("CARGO_PKG_VERSION"));
    let server = LdsServer::new();
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    // Route to CLI mode when any argument is supplied; otherwise use the
    // existing MCP stdio serve path (preserves Auto session-start behaviour).
    if std::env::args_os().count() <= 1 {
        serve_mcp().await
    } else {
        cli::run()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lds_core::config::{Config, Paths, Recipes};

    /// Verify that `resolve_startup_global_dirs` merges sources in the correct
    /// priority order: global_justfile → config.recipes.dirs → env_dirs.
    ///
    /// This test exercises crux 1 by injecting all three sources and asserting
    /// that the resulting Vec preserves the expected ordering without skipping
    /// any source.
    #[test]
    fn test_resolve_startup_global_dirs_priority_order() {
        let cfg = Config {
            recipes: Recipes {
                dirs: vec![PathBuf::from("/config/dir1"), PathBuf::from("/config/dir2")],
            },
            paths: Paths {
                global_justfile: Some(PathBuf::from("/custom/justfile")),
            },
        };

        // env_var with two paths (colon-separated on Unix, semicolon on Windows).
        #[cfg(unix)]
        let env_val = OsString::from("/env/dir1:/env/dir2");
        #[cfg(windows)]
        let env_val = OsString::from("/env/dir1;/env/dir2");

        let dirs = resolve_startup_global_dirs(cfg, Some(env_val));

        // Expected order:
        //   [0] global_justfile path (highest)
        //   [1] config dir 1
        //   [2] config dir 2
        //   [3] env dir 1 (lowest from env)
        //   [4] env dir 2
        assert_eq!(dirs.len(), 5, "expected 5 entries, got: {dirs:?}");
        assert_eq!(dirs[0], PathBuf::from("/custom/justfile"));
        assert_eq!(dirs[1], PathBuf::from("/config/dir1"));
        assert_eq!(dirs[2], PathBuf::from("/config/dir2"));
        assert_eq!(dirs[3], PathBuf::from("/env/dir1"));
        assert_eq!(dirs[4], PathBuf::from("/env/dir2"));
    }

    /// When no env_var is provided and config is default, the result is empty.
    #[test]
    fn test_resolve_startup_global_dirs_all_empty() {
        let dirs = resolve_startup_global_dirs(Config::default(), None);
        assert!(dirs.is_empty(), "expected empty dirs, got: {dirs:?}");
    }

    /// Only env var provided — config is default.
    #[test]
    fn test_resolve_startup_global_dirs_env_only() {
        #[cfg(unix)]
        let env_val = OsString::from("/env/only");
        #[cfg(windows)]
        let env_val = OsString::from("/env/only");

        let dirs = resolve_startup_global_dirs(Config::default(), Some(env_val));
        assert_eq!(dirs, vec![PathBuf::from("/env/only")]);
    }

    /// Only config.recipes.dirs provided — no env, no global_justfile.
    #[test]
    fn test_resolve_startup_global_dirs_config_only() {
        let cfg = Config {
            recipes: Recipes {
                dirs: vec![PathBuf::from("/config/only")],
            },
            paths: Paths {
                global_justfile: None,
            },
        };
        let dirs = resolve_startup_global_dirs(cfg, None);
        assert_eq!(dirs, vec![PathBuf::from("/config/only")]);
    }
}
