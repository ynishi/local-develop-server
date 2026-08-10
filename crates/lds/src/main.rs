mod cli;

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use lds_core::config::Config;
use lds_core::{LdsState, Session, SessionConfig, check_binaries};
use lds_gh::GhModule;
use lds_git::{GitModule, LogFilters, OtherStagedMode, ResetMode};
use lds_journal::JournalModule;
use lds_outline::OutlineModule;
use lds_publicity::{Platform, PublicityModule};
use lds_recipe::RecipeModule;
use lds_router::{ExportRegistry, McpRouter, RouteConfig};
use lds_sandbox::fs::SandboxFs;
use lds_sandbox::python::SandboxPython;
use rmcp::RoleServer;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    Annotated, CallToolRequestParams, CallToolResult, Content, ListResourceTemplatesResult,
    ListResourcesResult, ListToolsResult, PaginatedRequestParams, RawResource, RawResourceTemplate,
    ReadResourceRequestParams, ReadResourceResult, Resource, ResourceContents, ResourceTemplate,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::RwLock;
use tokio::task::spawn_blocking;

#[derive(Clone)]
struct LdsServer {
    state: Arc<RwLock<Inner>>,
}

struct Inner {
    lds: LdsState,
    git: Option<GitModule>,
    gh: Option<GhModule>,
    publicity: Option<PublicityModule>,
    recipe: Option<RecipeModule>,
    sandbox_fs: Option<SandboxFs>,
    sandbox_python: Option<SandboxPython>,
    journal: Option<JournalModule>,
    outline: Option<OutlineModule>,
    router: Option<McpRouter>,
    export_registry: Option<ExportRegistry>,
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
        let mut plugins = lds_recipe::list_global_plugins(&global_dirs)
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

    /// The session's currently materialized `[[export]]` tools, or an empty
    /// `Vec` if no session is active yet (mirrors `list_plugin_tools`'s
    /// no-session behavior: `list_tools` must still return the static
    /// surface before a session starts).
    async fn list_export_tools(&self) -> Vec<Tool> {
        let inner = self.state.read().await;
        let Some(registry) = inner.export_registry.clone() else {
            return Vec::new();
        };
        drop(inner);
        registry.list_tools().await
    }

    /// If `name` matches a materialized export tool's public name, dispatch
    /// it through the router to its upstream `(route, tool)` and return the
    /// result. Returns `Ok(None)` if `name` is not an export tool (so the
    /// caller falls through to plugin/static dispatch), or an error if
    /// export tools exist for a route whose router handle has since gone
    /// missing (should not happen in practice: both are set together in
    /// `wire_router_and_exports`).
    async fn try_export_call(
        &self,
        name: &str,
        arguments: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Result<Option<CallToolResult>, McpError> {
        let inner = self.state.read().await;
        let Some(registry) = inner.export_registry.clone() else {
            return Ok(None);
        };
        let router = inner.router.clone();
        drop(inner);

        let Some((route, upstream_tool)) = registry.resolve(name).await else {
            return Ok(None);
        };
        let router = router.ok_or_else(no_session_error)?;
        let args = serde_json::Value::Object(arguments.cloned().unwrap_or_default());
        router
            .call(&route, &upstream_tool, args)
            .await
            .map(Some)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
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
        let mut plugins = lds_recipe::list_global_plugins(&global_dirs)
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
                gh: None,
                publicity: None,
                recipe: None,
                sandbox_fs: None,
                sandbox_python: None,
                journal: None,
                outline: None,
                router: None,
                export_registry: None,
                startup_cwd,
                startup_global_dirs,
            })),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SessionStartReq {
    root: String,
    /// Optional human-readable alias for the default session. Enables
    /// `session_describe` / `session_doctor` / `session_close` /
    /// `session_alias_set` to address the session by alias.
    #[serde(default)]
    alias: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    max_output: Option<usize>,
    #[serde(default)]
    global_recipe_dir: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SessionCreateReq {
    /// Project root for the new session.
    root: String,
    /// Optional human-readable alias for later dispatch.
    #[serde(default)]
    alias: Option<String>,
    /// When true, the new session becomes the implicit default for
    /// backward-compat tool calls that omit `session_id`.
    #[serde(default)]
    make_default: bool,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    max_output: Option<usize>,
    #[serde(default)]
    global_recipe_dir: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SessionKeyReq {
    /// session_id or alias.
    key: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SessionAliasSetReq {
    /// session_id or current alias of the target session.
    key: String,
    /// New alias to assign.
    alias: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SessionAliasUnsetReq {
    /// Alias to remove (session itself is preserved).
    alias: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SessionDoctorReq {
    /// session_id or alias. Use "all" to run doctor on every session.
    #[serde(default)]
    key: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PublicityReq {
    /// Optional platform label. Recognised: `github` / `gh` / `crates` /
    /// `crates.io` / `cargo`. When omitted, every applicable platform is
    /// probed (github always; crates when Cargo.toml is present).
    #[serde(default)]
    platform: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GitLogReq {
    #[serde(default = "default_max_count")]
    max_count: usize,
    /// Case-sensitive substring against `"Name <email>"`.
    #[serde(default)]
    author: Option<String>,
    /// Git pathspec entries; a commit is kept when it touches at least one.
    #[serde(default)]
    paths: Option<Vec<String>>,
    /// Cutoff for commit author time. Accepts either a unix epoch integer
    /// (seconds) or an RFC 3339 timestamp string (e.g. `"2026-07-14T00:00:00Z"`
    /// or `"2026-07-14T09:00:00+09:00"`).
    #[serde(default)]
    since: Option<SinceInput>,
    /// Case-sensitive substring against the full commit message.
    #[serde(default)]
    grep: Option<String>,
}

/// Wire-level shape for `since`: an epoch integer or an RFC 3339 string.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
enum SinceInput {
    Epoch(i64),
    Rfc3339(String),
}

impl SinceInput {
    fn to_epoch(&self) -> Result<i64, String> {
        match self {
            SinceInput::Epoch(n) => Ok(*n),
            SinceInput::Rfc3339(s) => parse_since_string(s),
        }
    }
}

fn parse_since_string(s: &str) -> Result<i64, String> {
    let trimmed = s.trim();
    if let Ok(n) = trimmed.parse::<i64>() {
        return Ok(n);
    }
    chrono::DateTime::parse_from_rfc3339(trimmed)
        .map(|dt| dt.timestamp())
        .map_err(|e| format!("since: not a valid unix epoch or RFC 3339 timestamp: {e}"))
}

fn default_max_count() -> usize {
    20
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GitCommitReq {
    /// Working directory inside a session-owned worktree.
    working_dir: String,
    /// Commit message (single `-m`, so newlines land verbatim).
    message: String,
    /// Paths to commit. When omitted (or empty) every change is swept via
    /// `git add -A` (backward-compatible default). When set to a non-empty
    /// list, the commit contains exactly those paths and `other_staged`
    /// governs how the index's spillover is handled.
    #[serde(default)]
    paths: Option<Vec<String>>,
    /// `"stop"` (default) or `"restage"`. Consulted only when `only` is a
    /// non-empty list *and* the index already carries paths outside it.
    /// `stop` fails without touching state; `restage` unstages the
    /// intruders, commits `only`, then re-stages the intruders so the
    /// caller's pre-existing staged work survives the round-trip.
    #[serde(default)]
    other_staged: Option<String>,
    /// `false` (default) enables the dotfile / dot-dir safeguard: untracked
    /// entries whose path contains a `.`-prefixed component and are not in
    /// `.gitignore` are skipped from staging with a warning; tracked
    /// dotfile changes are committed but reported so pre-publish review can
    /// catch unintended edits to `.gitignore` / workflow files / etc.
    /// `true` suppresses the mechanism — every candidate is staged
    /// verbatim and no warnings are emitted.
    #[serde(default)]
    force_dot: Option<bool>,
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
struct GitFetchReq {
    #[serde(default)]
    remote: Option<String>,
    #[serde(default)]
    refspec: Option<String>,
    #[serde(default)]
    prune: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GitBranchStatusReq {
    branch: String,
    base: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GitUnpushedCommitsReq {
    branch: String,
    #[serde(default = "default_origin")]
    remote: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GitIsPushedReq {
    commit: String,
    #[serde(default = "default_origin")]
    remote: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GitTagPushedReq {
    tag: String,
    #[serde(default = "default_origin")]
    remote: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GitWorktreeStateReq {
    #[serde(default)]
    branch: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GitDiffReq {
    /// `true` -> `git diff --cached` (HEAD vs index).
    /// `false` (default) -> `git diff` (index vs worktree).
    #[serde(default)]
    staged: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GitResetReq {
    /// Working directory inside a session-owned worktree.
    working_dir: String,
    /// `"soft"` | `"mixed"` | `"hard"`.
    mode: String,
    /// Revspec or sha to move HEAD to.
    target: String,
    /// `false` (default) keeps the stash guard: `mode="hard"` is refused when
    /// stash entries exist and the working tree is dirty. `true` proceeds
    /// anyway.
    #[serde(default)]
    force: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GitStashShowReq {
    /// Position in the stash list (`stash@{index}`), 0 == most recent.
    index: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GitStashRestoreReq {
    /// Working directory inside a session-owned worktree.
    working_dir: String,
    /// Object id of the stash commit to put back (>= 7 hex chars) — normally
    /// the `dropped_sha` reported by `git_stash_finalize`. Revspecs such as
    /// `HEAD` or a branch name are rejected.
    sha: String,
    /// Reflog message for the restored entry. Defaults to the stash commit's
    /// own summary, i.e. the message the entry had before it was dropped.
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GitStashTxReq {
    /// Working directory inside a session-owned worktree.
    working_dir: String,
    /// Position in the stash list (`stash@{index}`), 0 == most recent.
    index: usize,
    /// Sha (full or >= 7-char prefix) the caller expects at `index`. Indexes
    /// shift whenever an entry is dropped; passing the sha from an earlier
    /// `git_stash_list` turns a silent wrong-entry operation into an error.
    #[serde(default)]
    expected_sha: Option<String>,
}

fn default_origin() -> String {
    "origin".to_string()
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

fn default_limit_30() -> usize {
    30
}

fn default_tail_20() -> usize {
    20
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GhPrListReq {
    #[serde(default = "default_limit_30")]
    limit: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GhPrViewReq {
    number: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GhPrDiffReq {
    number: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GhIssueListReq {
    #[serde(default = "default_limit_30")]
    limit: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GhIssueViewReq {
    number: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GhRunListReq {
    #[serde(default = "default_limit_30")]
    limit: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GhRunViewReq {
    run_id: u64,
    #[serde(default)]
    repo: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GhRunLogFailedReq {
    run_id: u64,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default = "default_tail_20")]
    tail_lines: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GhRunJobsReq {
    run_id: u64,
    #[serde(default)]
    repo: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GhReleaseViewReq {
    tag: String,
    #[serde(default)]
    repo: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GhReleaseListReq {
    #[serde(default = "default_limit_30")]
    limit: usize,
    #[serde(default)]
    repo: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GhWorkflowListReq {
    #[serde(default)]
    repo: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GhWorkflowViewReq {
    name_or_id: String,
    #[serde(default)]
    repo: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GhPrChecksReq {
    number: u64,
    #[serde(default)]
    repo: Option<String>,
}

/// Default `args` for [`McpCallReq`] when the caller omits the field: an
/// empty JSON object, which every upstream tool interprets as "no arguments".
fn default_mcp_call_args() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

#[derive(Debug, Deserialize, JsonSchema)]
struct McpCallReq {
    /// `<route>://<tool>` URI identifying the upstream route and tool.
    uri: String,
    /// Arguments forwarded verbatim to the upstream tool. Must be a JSON
    /// object; omitting the field defaults to an empty object.
    #[serde(default = "default_mcp_call_args")]
    args: serde_json::Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct McpRouteRegisterReq {
    /// Unique route name; the `<route>` component of `<route>://<tool>` URIs.
    name: String,
    /// Subprocess command to spawn (resolved via `PATH`).
    command: String,
    /// Command-line arguments passed to `command`.
    #[serde(default)]
    args: Vec<String>,
    /// Extra environment variables set on the spawned subprocess.
    #[serde(default)]
    env: HashMap<String, String>,
    /// Per-call timeout, in seconds. Defaults to 30 (matching
    /// `config.toml`'s `[[route]]` default) when omitted.
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct McpRouteRemoveReq {
    /// Name of the route to remove.
    name: String,
}

/// Shared factory for the "no session active" MCP error.
///
/// All tool handlers that require an active session use this factory so that
/// the error code (-32603) and message ("no session") are defined in one place.
/// Infallible — never fails.
fn no_session_error() -> McpError {
    McpError::internal_error("no session", None)
}

/// Parse the `other_staged` wire string into [`OtherStagedMode`]. `None` /
/// missing → default (`Stop`); unknown value → hard error so a typo doesn't
/// silently fall back to a mode the caller didn't intend.
fn parse_other_staged(mode: Option<&str>) -> Result<OtherStagedMode, McpError> {
    match mode {
        None | Some("") => Ok(OtherStagedMode::default()),
        Some("stop") => Ok(OtherStagedMode::Stop),
        Some("restage") => Ok(OtherStagedMode::Restage),
        Some(other) => Err(McpError::internal_error(
            format!("unknown other_staged mode {other:?} (expected stop|restage)"),
            None,
        )),
    }
}

/// Serialise a tool result into a [`CallToolResult`] whose single text block
/// is pretty-printed JSON.
///
/// Every typed handler funnels through here so the wire shape is uniform:
/// `Content::text(serde_json::to_string_pretty(&out)?)`. Inlining this at
/// each handler would duplicate the same map_err six lines deep across ~30
/// call sites and let formatting drift between them.
fn json_result<T: serde::Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![Content::text(text)]))
}

/// Resolve the user-global `config.toml` path (`~/.config/lds/config.toml`),
/// the same file [`Config::load_or_default`] reads for `[recipes]`/`[paths]`.
///
/// Falls back to a path that cannot exist if the home directory cannot be
/// determined, so [`RouteConfig::load`]'s "missing file → empty declaration
/// set" behavior degrades gracefully instead of reading an unrelated file.
fn user_config_path() -> PathBuf {
    lds_core::config::user_config_path()
        .unwrap_or_else(|| PathBuf::from("/nonexistent-home/.config/lds/config.toml"))
}

/// Build a session and its local (non-network) modules on `inner`.
///
/// This is the fast, synchronous half of session construction: starting the
/// session and wiring `git` / `gh` / `recipe` / `sandbox_fs` /
/// `sandbox_python` / `journal` touches only local state (no `.await`, no
/// upstream I/O). Callers hold `Inner`'s write lock for the duration of this
/// call and nothing more; [`wire_router_and_exports`] performs the
/// network-bound half (route/export config load + upstream `list_tools`
/// calls) separately, after the caller has dropped that write lock.
///
/// Shared as a single function between the explicit `session_start` handler
/// and the auto-start hook in `call_tool` so the two paths cannot diverge,
/// preserving session invariants (crux §1).
fn start_session_locally(
    inner: &mut Inner,
    config: SessionConfig,
) -> Result<Arc<Session>, McpError> {
    let session = inner
        .lds
        .start_session(config)
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    inner.git = Some(GitModule::new(Arc::clone(&session)));
    inner.gh = Some(GhModule::new(Arc::clone(&session)));
    inner.publicity = Some(PublicityModule::new(Arc::clone(&session)));
    inner.recipe = Some(RecipeModule::new(Arc::clone(&session)));
    inner.sandbox_fs = Some(
        SandboxFs::new(session.root())
            .map_err(|e| McpError::internal_error(e.to_string(), None))?,
    );
    inner.sandbox_python = Some(SandboxPython::new(session.root()));
    inner.journal = Some({
        // Opt-in file projection: LDS_JOURNAL_FILE_ENABLE turns it on; the
        // default output path lives under the session root but can be
        // overridden via LDS_JOURNAL_FILE_OUTPUT_PATH (relative paths
        // resolve against session.root()). Startup-time attachment is
        // passed to the upstream journal-mcp-rmcp server via
        // RunConfig.file_projection; runtime attach/detach continues to
        // work through the `journal_projection_*` MCP tools.
        let file_projection = if std::env::var_os("LDS_JOURNAL_FILE_ENABLE").is_some() {
            match std::env::var_os("LDS_JOURNAL_FILE_OUTPUT_PATH") {
                Some(raw) => {
                    let p = PathBuf::from(raw);
                    Some(if p.is_absolute() {
                        p
                    } else {
                        session.root().join(p)
                    })
                }
                None => Some(session.root().join("workspace").join("journal.md")),
            }
        } else {
            None
        };
        JournalModule::new(session.root().to_path_buf(), file_projection)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
    });
    // Outline module: shelf root defaults to $HOME/.config/outline-mcp/books
    // (matches the standalone outline-mcp binary — Outline Books are a
    // global SoT shared across projects, not per-session). Override via
    // LDS_OUTLINE_SHELF_DIR when a session wants an isolated shelf. Init
    // failure downgrades to None (`outline_*` tools become unavailable);
    // this keeps session_start resilient to a filesystem hiccup on the
    // shelf directory rather than aborting the whole session.
    inner.outline = {
        let shelf_dir = std::env::var_os("LDS_OUTLINE_SHELF_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(".config/outline-mcp/books")
            });
        match OutlineModule::new(shelf_dir.clone()) {
            Ok(m) => Some(m),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    shelf_dir = %shelf_dir.display(),
                    "OutlineModule init failed; outline_* tools disabled for this session"
                );
                None
            }
        }
    };
    Ok(session)
}

/// Load `session`'s route/export config and materialize its export tool
/// registry, then publish both onto `state` under a short-lived write lock.
///
/// # Concurrency
/// All upstream network I/O — `RouteConfig::load_all`'s filesystem read (via
/// `spawn_blocking`) and `ExportRegistry::refresh`'s upstream `list_tools`
/// calls (subprocess spawn + MCP round trip per declared route) — runs
/// without holding `Inner`'s write lock, so a slow or unreachable route
/// cannot block every other concurrent tool call for the duration (R3;
/// Outline `rust` book §4-1, K-4; mirrors `mcp_call`'s clone-then-drop
/// pattern below). The write lock is reacquired only long enough to assign
/// the two already-built values onto `inner.router` / `inner.export_registry`
/// — no `.await` is held across it.
async fn wire_router_and_exports(
    state: &Arc<RwLock<Inner>>,
    session: &Arc<Session>,
) -> Result<(), McpError> {
    // Route + export config: `[[route]]`/`[[export]]` sections of the
    // user-global `~/.config/lds/config.toml`, overridden by the
    // project-local `<session_root>/config.toml` — the same two files
    // `Config::load_or_default` reads for `[recipes]`/`[paths]`.
    // `RouteConfig::load_all` performs synchronous filesystem I/O (see its
    // doc comment), so it is run on a blocking-pool thread rather than
    // inline in this async fn.
    let user_path = user_config_path();
    let project_path = session.root().join("config.toml");
    let session_root = session.root().to_path_buf();
    let (routes, exports) =
        spawn_blocking(move || RouteConfig::load_all(&user_path, &project_path, &session_root))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    let router = McpRouter::from_configs(routes);

    // Export registry: re-fetch each `[[export]]` declaration's upstream
    // tool list and materialize it under a prefixed public name.
    // `ExportLimitExceeded`/`ExportCollision` are propagated as
    // session_start failures (a config.toml that cannot be resolved into an
    // unambiguous tool surface); a single unreachable route's declaration is
    // instead logged and skipped inside `ExportRegistry::refresh` so it does
    // not take down an otherwise-healthy session.
    let static_tool_names: Vec<String> = LdsServer::tool_router()
        .list_all()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    let export_registry = ExportRegistry::from_declarations(exports);
    export_registry
        .refresh(&router, &static_tool_names)
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

    let mut inner = state.write().await;
    inner.router = Some(router);
    inner.export_registry = Some(export_registry);
    Ok(())
}

/// Return `true` if `path` is a ProjectRoot: a directory that contains a
/// `.git` entry or a `justfile`. Conventional project-root probe.
fn is_project_root(path: &std::path::Path) -> bool {
    path.join(".git").exists() || path.join("justfile").exists()
}

/// If no session is active yet and the server was launched from a
/// ProjectRoot, start one automatically from `startup_cwd` and materialize
/// its routes/exports.
///
/// Shared by the `call_tool` auto-start hook and `serve_mcp`'s eager
/// startup call so the two paths cannot diverge (crux §1): whichever caller
/// reaches this first wins, and any caller that finds a session already
/// active is a no-op (`Ok(None)`).
///
/// # Concurrency
/// The session-creation decision (`session().is_err()` check) and
/// `start_session_locally` run inside the same write-lock scope so
/// concurrent callers cannot double-start a session; the network-bound
/// `wire_router_and_exports` call runs after the lock is dropped, per its
/// own doc comment.
async fn maybe_auto_start_session(
    state: &Arc<RwLock<Inner>>,
) -> Result<Option<Arc<Session>>, McpError> {
    let started_session = {
        let mut inner = state.write().await;
        if inner.lds.session().is_err()
            && let Some(cwd) = inner.startup_cwd.clone()
            && is_project_root(&cwd)
        {
            // Auto-start: use startup_global_dirs (config.toml + env) so plugins are resolved correctly.
            let global_recipe_dirs = (*inner.startup_global_dirs).clone();
            let config = SessionConfig {
                root: cwd,
                global_recipe_dirs,
                ..Default::default()
            };
            Some(start_session_locally(&mut inner, config)?)
        } else {
            None
        }
        // write guard drops here — before wire_router_and_exports's upstream
        // network `.await`s below
    };
    if let Some(session) = &started_session {
        wire_router_and_exports(state, session).await?;
    }
    Ok(started_session)
}

#[tool_router]
impl LdsServer {
    #[tool(
        description = "Initialize session with project root (with optional alias). Must be called first. Replaces the implicit default session."
    )]
    async fn session_start(
        &self,
        Parameters(req): Parameters<SessionStartReq>,
        peer: rmcp::Peer<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // Adapter: compose global_recipe_dirs from the MCP single arg (if any)
        // followed by startup_global_dirs (config.toml dirs then env dirs) in
        // declaration order.
        // Precedence (low→high): default ~/.config/lds → config.toml dirs → env dirs → MCP wire arg → project.
        let config = {
            let inner = self.state.read().await;
            let startup_dirs = inner.startup_global_dirs.clone();
            let mut global_recipe_dirs: Vec<PathBuf> = req
                .global_recipe_dir
                .map(|s| vec![PathBuf::from(s)])
                .unwrap_or_default();
            global_recipe_dirs.extend(startup_dirs.iter().cloned());
            SessionConfig {
                root: req.root.into(),
                timeout_secs: req.timeout_secs,
                max_output: req.max_output,
                alias: req.alias.clone(),
                global_recipe_dirs,
                worktrees_dir: None,
            }
        };
        // Local module construction takes a short-lived write lock; the
        // route/export network I/O runs after that lock is dropped (see
        // `wire_router_and_exports`'s doc comment).
        let session = {
            let mut inner = self.state.write().await;
            start_session_locally(&mut inner, config)?
        };
        wire_router_and_exports(&self.state, &session).await?;
        // Best-effort: a client that doesn't advertise the notifications
        // capability simply ignores this; failures here must not fail the
        // session_start response itself.
        if let Err(e) = peer.notify_tool_list_changed().await {
            tracing::warn!(error = %e, "notify_tool_list_changed failed after session_start");
        }
        json_result(&serde_json::json!({
            "session_id": session.id(),
            "alias": session.alias(),
            "root": session.root().display().to_string(),
            "is_default": true,
        }))
    }

    #[tool(
        description = "Show git working tree status as JSON (branch, head_sha, staged/unstaged/untracked arrays, clean flag)"
    )]
    async fn git_status(&self) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner.git.as_ref().ok_or_else(no_session_error)?;
        let out = git
            .status()
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
    }

    #[tool(
        description = "Show git commit log as JSON ({commits: [{sha, short_sha, summary, author, timestamp}]}). \
Filters (all optional, AND-combined): `author` (case-sensitive substring against \"Name <email>\"), \
`paths` (git pathspec list; commit kept when it touches at least one entry), \
`since` (cutoff for commit.author_time; accepts unix epoch integer or RFC 3339 string like \"2026-07-14T00:00:00Z\"), \
`grep` (case-sensitive substring against the full commit message). \
`max_count` caps the post-filter result count."
    )]
    async fn git_log(
        &self,
        Parameters(req): Parameters<GitLogReq>,
    ) -> Result<CallToolResult, McpError> {
        let since = match req.since {
            Some(input) => Some(
                input
                    .to_epoch()
                    .map_err(|e| McpError::invalid_params(e, None))?,
            ),
            None => None,
        };
        let inner = self.state.read().await;
        let git = inner.git.as_ref().ok_or_else(no_session_error)?;
        let filters = LogFilters {
            max_count: req.max_count,
            author: req.author,
            paths: req.paths,
            since,
            grep: req.grep,
        };
        let out = git
            .log(filters)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
    }

    #[tool(
        description = "Show git diff as JSON. staged=false (default) → index vs worktree; staged=true → HEAD vs index (git diff --cached)"
    )]
    async fn git_diff(
        &self,
        Parameters(req): Parameters<GitDiffReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner.git.as_ref().ok_or_else(no_session_error)?;
        let out = git
            .diff(req.staged)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
    }

    #[tool(description = "List git worktrees as JSON, each with session ownership flag")]
    async fn git_worktree_list(&self) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner.git.as_ref().ok_or_else(no_session_error)?;
        let out = git
            .worktree_list()
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
    }

    #[tool(description = "Create a session-owned git worktree on a new branch. \
                       The worktree is placed under the session-scoped worktrees dir \
                       resolved by SessionConfig::worktrees_dir (explicit config → \
                       env LDS_WORKTREES_DIR → session default). The target must be \
                       gitignored in the parent repo.")]
    async fn git_worktree_add(
        &self,
        Parameters(req): Parameters<GitWorktreeAddReq>,
    ) -> Result<CallToolResult, McpError> {
        let mut inner = self.state.write().await;
        let git = inner.git.as_mut().ok_or_else(no_session_error)?;
        let out = git
            .worktree_add(&req.name, &req.branch, req.base_branch.as_deref())
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
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
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
    }

    #[tool(description = "Commit changes in a session-owned working directory. \
`paths` omitted or empty: sweeps every change via `git add -A`. \
`paths` set: commits exactly those paths; when the index carries other \
staged paths, `other_staged` decides — `stop` (default) fails leaving \
state unchanged, `restage` unstages the intruders, commits `paths`, then \
re-stages them so pre-existing staged work survives. Dotfile / dot-dir \
safeguard is on by default: untracked-and-not-in-`.gitignore` entries \
(`.env`, freshly-dropped `.claude/*`, `foo/.hidden`, etc.) are skipped \
from staging and reported in the response `dotfile_warnings` / \
`dotfile_skipped`; tracked dotfile changes are still committed but \
reported the same way so pre-publish review can catch unintended edits \
to `.gitignore` / `.github/workflows/*.yml` / etc. Pass `force_dot: true` \
to suppress the safeguard entirely.")]
    async fn git_commit(
        &self,
        Parameters(req): Parameters<GitCommitReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner.git.as_ref().ok_or_else(no_session_error)?;
        let working_dir = PathBuf::from(&req.working_dir);
        let other_staged = parse_other_staged(req.other_staged.as_deref())?;
        let out = git
            .commit(
                &working_dir,
                &req.message,
                req.paths.as_deref(),
                other_staged,
                req.force_dot.unwrap_or(false),
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
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
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
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
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
    }

    #[tool(description = "Fetch from a remote (default origin)")]
    async fn git_fetch(
        &self,
        Parameters(req): Parameters<GitFetchReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner.git.as_ref().ok_or_else(no_session_error)?;
        let out = git
            .fetch(req.remote.as_deref(), req.refspec.as_deref(), req.prune)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
    }

    #[tool(description = "List git remotes as JSON ({remotes: [{name, fetch_url, push_url}]})")]
    async fn git_remote_list(&self) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner.git.as_ref().ok_or_else(no_session_error)?;
        let out = git
            .remote_list()
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
    }

    #[tool(description = "Show ahead/behind counts between branch and base (e.g. origin/main)")]
    async fn git_branch_status(
        &self,
        Parameters(req): Parameters<GitBranchStatusReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner.git.as_ref().ok_or_else(no_session_error)?;
        let out = git
            .branch_status(&req.branch, &req.base)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
    }

    #[tool(description = "List commits on branch not yet pushed to <remote>/<branch>")]
    async fn git_unpushed_commits(
        &self,
        Parameters(req): Parameters<GitUnpushedCommitsReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner.git.as_ref().ok_or_else(no_session_error)?;
        let out = git
            .unpushed_commits(&req.branch, &req.remote)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
    }

    #[tool(description = "Check whether a commit is reachable from any remote ref")]
    async fn git_is_pushed(
        &self,
        Parameters(req): Parameters<GitIsPushedReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner.git.as_ref().ok_or_else(no_session_error)?;
        let out = git
            .is_pushed(&req.commit, &req.remote)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
    }

    #[tool(description = "Check whether a tag is pushed to a remote via git ls-remote --tags")]
    async fn git_tag_pushed(
        &self,
        Parameters(req): Parameters<GitTagPushedReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner.git.as_ref().ok_or_else(no_session_error)?;
        let out = git
            .tag_pushed(&req.tag, &req.remote)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
    }

    #[tool(
        description = "Snapshot of a worktree state as JSON (branch, tracking, ahead, behind, uncommitted, clean, sync)"
    )]
    async fn git_worktree_state(
        &self,
        Parameters(req): Parameters<GitWorktreeStateReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner.git.as_ref().ok_or_else(no_session_error)?;
        let out = git
            .worktree_state(req.branch.as_deref())
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
    }

    #[tool(
        description = "Reset HEAD in a session-owned working directory. mode = soft | mixed | hard. \
mode=hard is refused when the repo has stash entries AND the working tree is dirty — that shape \
means an applied stash is in flight, and --hard would destroy it without a trace. Undo the apply \
with git_stash_abort (the entry survives), or pass force=true when the reset is genuinely intended."
    )]
    async fn git_reset(
        &self,
        Parameters(req): Parameters<GitResetReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner.git.as_ref().ok_or_else(no_session_error)?;
        let mode = match req.mode.as_str() {
            "soft" => ResetMode::Soft,
            "mixed" => ResetMode::Mixed,
            "hard" => ResetMode::Hard,
            other => {
                return Err(McpError::internal_error(
                    format!("unknown reset mode {other:?} (expected soft|mixed|hard)"),
                    None,
                ));
            }
        };
        let working_dir = PathBuf::from(&req.working_dir);
        let out = git
            .reset(&working_dir, mode, &req.target, req.force)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
    }

    #[tool(
        description = "List git stash entries as JSON ({stashes: [{index, sha, message, has_untracked}]}). \
Read-only. `sha` is stable while `index` shifts on every drop — carry the sha into the \
apply / abort / finalize calls as expected_sha."
    )]
    async fn git_stash_list(&self) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner.git.as_ref().ok_or_else(no_session_error)?;
        let out = git
            .stash_list()
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
    }

    #[tool(
        description = "Inspect one stash entry without applying it: patch against the commit it was \
taken on, the tracked paths it touches, and the untracked paths it carries (git stash push -u). \
Read-only."
    )]
    async fn git_stash_show(
        &self,
        Parameters(req): Parameters<GitStashShowReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner.git.as_ref().ok_or_else(no_session_error)?;
        let out = git
            .stash_show(req.index)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
    }

    #[tool(
        description = "Apply a stash entry into a session-owned working directory, KEEPING the entry \
(step 1 of 2). Requires a clean working tree: any staged or unstaged change is refused so the \
applied content never mixes with hand edits (untracked files are allowed). Also refused when the \
entry's untracked files already exist on disk. On conflict the working tree is rolled back \
automatically and the entry is left intact. Verify the result, then call git_stash_finalize to \
drop the entry, or git_stash_abort to undo. There is no pop tool by design — pop would drop the \
entry before anyone could check the apply."
    )]
    async fn git_stash_apply(
        &self,
        Parameters(req): Parameters<GitStashTxReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner.git.as_ref().ok_or_else(no_session_error)?;
        let working_dir = PathBuf::from(&req.working_dir);
        let out = git
            .stash_apply(&working_dir, req.index, req.expected_sha)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
    }

    #[tool(
        description = "Undo an applied stash: every path the entry touches goes back to its HEAD \
state and the entry itself is kept. Edits layered on top of the applied stash are discarded too \
(the apply's clean-tree precondition is what makes that safe). Use this instead of \
git_reset mode=hard when a stash apply turned out wrong."
    )]
    async fn git_stash_abort(
        &self,
        Parameters(req): Parameters<GitStashTxReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner.git.as_ref().ok_or_else(no_session_error)?;
        let working_dir = PathBuf::from(&req.working_dir);
        let out = git
            .stash_abort(&working_dir, req.index, req.expected_sha)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
    }

    #[tool(
        description = "Drop a stash entry after its applied content has been verified (step 2 of 2). \
Returns dropped_sha — the stash commit stays reachable until git gc prunes it, so \
`git stash apply <dropped_sha>` still recovers the content. Record it; it is the difference \
between dropped and lost. This is the only drop path: bare drop / clear are not exposed."
    )]
    async fn git_stash_finalize(
        &self,
        Parameters(req): Parameters<GitStashTxReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner.git.as_ref().ok_or_else(no_session_error)?;
        let working_dir = PathBuf::from(&req.working_dir);
        let out = git
            .stash_finalize(&working_dir, req.index, req.expected_sha)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
    }

    #[tool(
        description = "Put a dropped stash entry back on refs/stash: pass the dropped_sha that \
git_stash_finalize returned. A drop only removes the reflog entry — the commit itself survives \
unreferenced until git gc prunes it, so this works until gc runs (and not after). The restored \
entry lands at stash@{0}, keeping its original message unless `message` overrides it. Refused \
when the sha is not stash-shaped (>= 2 parents) or is already in the stash list."
    )]
    async fn git_stash_restore(
        &self,
        Parameters(req): Parameters<GitStashRestoreReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner.git.as_ref().ok_or_else(no_session_error)?;
        let working_dir = PathBuf::from(&req.working_dir);
        let out = git
            .stash_restore(&working_dir, &req.sha, req.message)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
    }

    #[tool(
        description = "Adopt orphan worktrees under the session-scoped worktrees \
                       dir (see git_worktree_add) that were left behind by a \
                       previous session."
    )]
    async fn git_session_release(&self) -> Result<CallToolResult, McpError> {
        let mut inner = self.state.write().await;
        let git = inner.git.as_mut().ok_or_else(no_session_error)?;
        let out = git
            .session_release()
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
    }

    /// Check gh CLI authentication status.
    #[tool(description = "Check gh CLI authentication status")]
    async fn gh_auth_status(&self) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let gh = inner.gh.as_ref().ok_or_else(no_session_error)?;
        let out = gh
            .auth_status()
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// List GitHub pull requests (read-only).
    #[tool(
        description = "List GitHub pull requests (read-only). Returns JSON array of PRs with number, title, state, author."
    )]
    async fn gh_pr_list(
        &self,
        Parameters(req): Parameters<GhPrListReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let gh = inner.gh.as_ref().ok_or_else(no_session_error)?;
        let out = gh
            .pr_list(req.limit)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// View a single GitHub pull request as JSON (read-only).
    #[tool(description = "View a single GitHub pull request as JSON (read-only).")]
    async fn gh_pr_view(
        &self,
        Parameters(req): Parameters<GhPrViewReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let gh = inner.gh.as_ref().ok_or_else(no_session_error)?;
        let out = gh
            .pr_view(req.number)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// Show diff of a GitHub pull request (read-only).
    #[tool(description = "Show diff of a GitHub pull request (read-only).")]
    async fn gh_pr_diff(
        &self,
        Parameters(req): Parameters<GhPrDiffReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let gh = inner.gh.as_ref().ok_or_else(no_session_error)?;
        let out = gh
            .pr_diff(req.number)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// List GitHub issues (read-only).
    #[tool(
        description = "List GitHub issues (read-only). Returns JSON array of issues with number, title, state."
    )]
    async fn gh_issue_list(
        &self,
        Parameters(req): Parameters<GhIssueListReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let gh = inner.gh.as_ref().ok_or_else(no_session_error)?;
        let out = gh
            .issue_list(req.limit)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// View a single GitHub issue as JSON (read-only).
    #[tool(description = "View a single GitHub issue as JSON (read-only).")]
    async fn gh_issue_view(
        &self,
        Parameters(req): Parameters<GhIssueViewReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let gh = inner.gh.as_ref().ok_or_else(no_session_error)?;
        let out = gh
            .issue_view(req.number)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// View repository metadata as JSON (read-only).
    #[tool(description = "View repository metadata as JSON (read-only).")]
    async fn gh_repo_view(&self) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let gh = inner.gh.as_ref().ok_or_else(no_session_error)?;
        let out = gh
            .repo_view()
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// Classify per-platform publicity (PUBLIC/PRIVATE/INTERNAL/LOCAL/FORKED/AMBIGUOUS/UNKNOWN).
    ///
    /// Without `platform`, probes every applicable platform (github always
    /// runs; crates runs when a Cargo.toml is present). With `platform`,
    /// probes only that one. Recognised values: `github` / `gh` /
    /// `crates` / `crates.io` / `cargo`. Returns
    /// `{ results: [{ platform, publicity, reason, detail }, ...] }`.
    #[tool(
        description = "Classify per-platform publicity for the current session root. Returns one result per platform (github / crates) with a canonical value in {PUBLIC, PRIVATE, INTERNAL, LOCAL, FORKED, AMBIGUOUS, UNKNOWN} plus a reason and detail JSON. Pass `platform` (optional: github / crates) to restrict to a single platform; omit to probe all applicable."
    )]
    async fn publicity(
        &self,
        Parameters(req): Parameters<PublicityReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let publicity = inner.publicity.as_ref().ok_or_else(no_session_error)?;
        let results = match req.platform.as_deref() {
            None => publicity.detect_all().await,
            Some(label) => {
                let p = Platform::parse(label).ok_or_else(|| {
                    McpError::invalid_params(
                        format!(
                            "unknown platform '{label}' (expected: github / gh / crates / crates.io / cargo)"
                        ),
                        None,
                    )
                })?;
                vec![publicity.detect(p).await]
            }
        };
        let body = serde_json::json!({ "results": results });
        let text = serde_json::to_string_pretty(&body)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// List GitHub Actions workflow runs (read-only).
    #[tool(
        description = "List GitHub Actions workflow runs (read-only). Returns JSON array with status, conclusion, workflowName."
    )]
    async fn gh_run_list(
        &self,
        Parameters(req): Parameters<GhRunListReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let gh = inner.gh.as_ref().ok_or_else(no_session_error)?;
        let out = gh
            .run_list(req.limit)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(
        description = "Get details of a single GitHub Actions workflow run (read-only). Returns JSON with run status, conclusion, and job summary."
    )]
    async fn gh_run_view(
        &self,
        Parameters(req): Parameters<GhRunViewReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let gh = inner.gh.as_ref().ok_or_else(no_session_error)?;
        let out = gh
            .run_view(req.run_id, req.repo)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(
        description = "Get logs of failed steps in a GitHub Actions workflow run (read-only). Returns JSON with failed_steps array containing job_name, step_name, and log_tail."
    )]
    async fn gh_run_log_failed(
        &self,
        Parameters(req): Parameters<GhRunLogFailedReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let gh = inner.gh.as_ref().ok_or_else(no_session_error)?;
        let out = gh
            .run_log_failed(req.run_id, req.repo, Some(req.tail_lines))
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(
        description = "List jobs of a GitHub Actions workflow run (read-only). Returns JSON array with job name, status, and step details."
    )]
    async fn gh_run_jobs(
        &self,
        Parameters(req): Parameters<GhRunJobsReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let gh = inner.gh.as_ref().ok_or_else(no_session_error)?;
        let out = gh
            .run_jobs(req.run_id, req.repo)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(
        description = "Get details of a GitHub release by tag (read-only). Returns JSON with tag name, release notes, and asset list."
    )]
    async fn gh_release_view(
        &self,
        Parameters(req): Parameters<GhReleaseViewReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let gh = inner.gh.as_ref().ok_or_else(no_session_error)?;
        let out = gh
            .release_view(req.tag, req.repo)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(
        description = "List GitHub releases (read-only). Returns JSON array with tag, name, and published date."
    )]
    async fn gh_release_list(
        &self,
        Parameters(req): Parameters<GhReleaseListReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let gh = inner.gh.as_ref().ok_or_else(no_session_error)?;
        let out = gh
            .release_list(req.limit, req.repo)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(
        description = "List GitHub Actions workflows in the repository (read-only). Returns JSON array with workflow name, id, and state."
    )]
    async fn gh_workflow_list(
        &self,
        Parameters(req): Parameters<GhWorkflowListReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let gh = inner.gh.as_ref().ok_or_else(no_session_error)?;
        let out = gh
            .workflow_list(req.repo)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(
        description = "Get details of a GitHub Actions workflow by name or id (read-only). Returns JSON with workflow metadata and recent run summary."
    )]
    async fn gh_workflow_view(
        &self,
        Parameters(req): Parameters<GhWorkflowViewReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let gh = inner.gh.as_ref().ok_or_else(no_session_error)?;
        let out = gh
            .workflow_view(req.name_or_id, req.repo)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(
        description = "List CI check runs for a GitHub pull request (read-only). Returns JSON array with check name, status, and conclusion."
    )]
    async fn gh_pr_checks(
        &self,
        Parameters(req): Parameters<GhPrChecksReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let gh = inner.gh.as_ref().ok_or_else(no_session_error)?;
        let out = gh
            .pr_checks(req.number, req.repo)
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

    // -----------------------------------------------------------------------
    // Multi-session ledger — 6 tools.
    //
    // session_start (above) remains the backward-compatible entry that always
    // replaces the implicit default session. The tools below let callers
    // spawn and address sessions explicitly by id or alias, and inspect /
    // diagnose them at any turn.
    // -----------------------------------------------------------------------

    #[tool(
        description = "Create an additional session bound to an arbitrary root, optionally with a human-readable alias. \
                       Existing default session is preserved unless make_default=true."
    )]
    async fn session_create(
        &self,
        Parameters(req): Parameters<SessionCreateReq>,
    ) -> Result<CallToolResult, McpError> {
        let mut inner = self.state.write().await;
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
            alias: req.alias.clone(),
            global_recipe_dirs,
            worktrees_dir: None,
        };
        let session = inner
            .lds
            .create_session(config, req.make_default)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&serde_json::json!({
            "session_id": session.id(),
            "alias": session.alias(),
            "root": session.root().display().to_string(),
            "is_default": inner.lds.default_session_id() == Some(session.id()),
        }))
    }

    #[tool(
        description = "List all live sessions in the ledger (id, alias, root, timestamps, is_default)."
    )]
    async fn session_list(&self) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let entries: Vec<serde_json::Value> = inner
            .lds
            .list_sessions()
            .into_iter()
            .map(|e| {
                serde_json::json!({
                    "session_id": e.session_id,
                    "alias": e.alias,
                    "root": e.root.display().to_string(),
                    "created_at": e.created_at,
                    "last_used_at": e.last_used_at,
                    "is_default": e.is_default,
                })
            })
            .collect();
        json_result(&serde_json::json!({ "sessions": entries }))
    }

    #[tool(
        description = "Call an external MCP tool via routed subprocess. uri = '<route>://<tool>'.",
        annotations(idempotent_hint = false)
    )]
    async fn mcp_call(
        &self,
        Parameters(req): Parameters<McpCallReq>,
    ) -> Result<CallToolResult, McpError> {
        if !req.args.is_object() {
            return Err(McpError::invalid_params("args must be an object", None));
        }
        // Read guard is held only long enough to clone the router handle;
        // dropped before the upstream `.await` so a slow/hung route never
        // blocks concurrent tool calls against `Inner` (R3/K-4, see
        // `lds_router::McpRouter` doc comment). The `lds://` self-loop guard
        // is enforced inside `McpRouter::call_uri` itself.
        let inner = self.state.read().await;
        let router = inner.router.clone().ok_or_else(no_session_error)?;
        drop(inner);
        router
            .call_uri(&req.uri, req.args)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        description = "List registered MCP routes for the active session.",
        annotations(idempotent_hint = true, destructive_hint = false)
    )]
    async fn mcp_route_list(&self) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let router = inner.router.clone().ok_or_else(no_session_error)?;
        drop(inner);
        let routes = router.list_routes().await;
        json_result(&serde_json::json!({ "routes": routes }))
    }

    #[tool(
        description = "Register or replace an MCP route. In-memory only; not persisted to config.toml.",
        annotations(idempotent_hint = true, destructive_hint = false)
    )]
    async fn mcp_route_register(
        &self,
        Parameters(req): Parameters<McpRouteRegisterReq>,
        peer: rmcp::Peer<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let router = inner.router.clone().ok_or_else(no_session_error)?;
        drop(inner);
        let name = req.name.clone();
        let route = RouteConfig {
            name: req.name,
            command: req.command,
            args: req.args,
            env: req.env,
            // Mirrors `config.toml`'s `[[route]]` default (30s); the router
            // crate's own default is a private helper, so it is duplicated
            // here rather than exposed as a new public API surface.
            timeout_secs: req.timeout_secs.unwrap_or(30),
        };
        router
            .register(route)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        // Best-effort: registering a route doesn't itself add tools, but a
        // route can be immediately followed by `mcp_call`, and callers that
        // cache tool lists benefit from a refresh nudge regardless.
        if let Err(e) = peer.notify_tool_list_changed().await {
            tracing::warn!(error = %e, "notify_tool_list_changed failed after mcp_route_register");
        }
        json_result(&serde_json::json!({ "registered": name }))
    }

    #[tool(
        description = "Remove an MCP route and terminate its subprocess.",
        annotations(idempotent_hint = true, destructive_hint = true)
    )]
    async fn mcp_route_remove(
        &self,
        Parameters(req): Parameters<McpRouteRemoveReq>,
        peer: rmcp::Peer<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let router = inner.router.clone().ok_or_else(no_session_error)?;
        drop(inner);
        router
            .remove(&req.name)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        // Best-effort: removing a route can also drop exports bound to it
        // (via a subsequent `mcp_export_refresh`), so nudge callers to
        // re-fetch their cached tool list.
        if let Err(e) = peer.notify_tool_list_changed().await {
            tracing::warn!(error = %e, "notify_tool_list_changed failed after mcp_route_remove");
        }
        json_result(&serde_json::json!({ "removed": req.name }))
    }

    #[tool(
        description = "List the session's currently materialized `[[export]]` tools.",
        annotations(idempotent_hint = true, destructive_hint = false)
    )]
    async fn mcp_export_list(&self) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let registry = inner.export_registry.clone().ok_or_else(no_session_error)?;
        drop(inner);
        let tools = registry.list_tools().await;
        json_result(&serde_json::json!({ "exports": tools }))
    }

    #[tool(
        description = "Re-fetch upstream tool schemas for every declared `[[export]]` route and replace the materialized export tool set.",
        annotations(idempotent_hint = false, destructive_hint = false)
    )]
    async fn mcp_export_refresh(
        &self,
        peer: rmcp::Peer<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let registry = inner.export_registry.clone().ok_or_else(no_session_error)?;
        let router = inner.router.clone().ok_or_else(no_session_error)?;
        drop(inner);
        let static_tool_names: Vec<String> = Self::tool_router()
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        registry
            .refresh(&router, &static_tool_names)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let tools = registry.list_tools().await;
        // Best-effort: this rebuilds the exported tool set, so callers with
        // a cached tool list should re-fetch it.
        if let Err(e) = peer.notify_tool_list_changed().await {
            tracing::warn!(error = %e, "notify_tool_list_changed failed after mcp_export_refresh");
        }
        json_result(&serde_json::json!({ "exports": tools }))
    }

    #[tool(description = "Describe a single session by id or alias.")]
    async fn session_describe(
        &self,
        Parameters(req): Parameters<SessionKeyReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let entry = inner
            .lds
            .describe(&req.key)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&serde_json::json!({
            "session_id": entry.session_id,
            "alias": entry.alias,
            "root": entry.root.display().to_string(),
            "created_at": entry.created_at,
            "last_used_at": entry.last_used_at,
            "is_default": entry.is_default,
        }))
    }

    #[tool(description = "Assign (or change) the alias of an existing session.")]
    async fn session_alias_set(
        &self,
        Parameters(req): Parameters<SessionAliasSetReq>,
    ) -> Result<CallToolResult, McpError> {
        let mut inner = self.state.write().await;
        inner
            .lds
            .set_alias(&req.key, req.alias.clone())
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&serde_json::json!({ "ok": true, "alias": req.alias }))
    }

    #[tool(description = "Remove an alias from the ledger; the underlying session is preserved.")]
    async fn session_alias_unset(
        &self,
        Parameters(req): Parameters<SessionAliasUnsetReq>,
    ) -> Result<CallToolResult, McpError> {
        let mut inner = self.state.write().await;
        inner
            .lds
            .unset_alias(&req.alias)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&serde_json::json!({ "ok": true }))
    }

    #[tool(description = "Close a session by id or alias and remove it from the ledger.")]
    async fn session_close(
        &self,
        Parameters(req): Parameters<SessionKeyReq>,
    ) -> Result<CallToolResult, McpError> {
        let mut inner = self.state.write().await;
        inner
            .lds
            .close(&req.key)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&serde_json::json!({ "ok": true }))
    }

    #[tool(
        description = "Run health checks (root-exists / git-bound / journal-db-writable / stale-lock / \
                       ownership-drift / root-conflict / ledger-leak) on one or every session. \
                       Pass key=\"all\" or omit to scan every session."
    )]
    async fn session_doctor(
        &self,
        Parameters(req): Parameters<SessionDoctorReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let key = req.key.unwrap_or_else(|| "all".to_string());
        let report_value = |r: &lds_core::DoctorReport| {
            let checks: Vec<serde_json::Value> = r
                .checks
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "name": c.name,
                        "status": c.status.as_str(),
                        "evidence": c.evidence,
                    })
                })
                .collect();
            serde_json::json!({
                "session_id": r.session_id,
                "alias": r.alias,
                "verdict": r.verdict.as_str(),
                "checks": checks,
            })
        };
        if key == "all" {
            let mut reports: Vec<serde_json::Value> = Vec::new();
            for entry in inner.lds.list_sessions() {
                let r = inner
                    .lds
                    .doctor(&entry.session_id)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                reports.push(report_value(&r));
            }
            json_result(&serde_json::json!({ "reports": reports }))
        } else {
            let r = inner
                .lds
                .doctor(&key)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            json_result(&report_value(&r))
        }
    }

    // Journal tools (17) are forwarded to journal-mcp-rmcp's OutlineMcpServer
    // via `JournalModule::list_tools` / `try_call` — see ServerHandler impl below.
}

#[tool_handler]
impl ServerHandler for LdsServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.instructions = Some("local-develop-server: unified MCP for orch pipeline".into());
        info.capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_tool_list_changed()
            .enable_resources()
            .build();
        info
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let mut tools = Self::tool_router().list_all();
        let plugins = self.list_plugin_tools().await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "list_plugin_tools failed during list_tools");
            Vec::new()
        });
        tools.extend(plugins);
        tools.extend(self.list_export_tools().await);
        // Outline + Journal tools: forwarded verbatim from their upstream
        // SDK servers. Outline tools carry an `outline_` prefix stamped on
        // in `list_tools_prefixed`; journal tools already carry the
        // `journal_` prefix in-crate so no rewriting is needed. Only
        // available once a session is started (modules live on Inner).
        {
            let inner = self.state.read().await;
            if let Some(outline) = inner.outline.as_ref() {
                match outline
                    .list_tools_prefixed(request.clone(), context.clone())
                    .await
                {
                    Ok(mut outline_tools) => tools.append(&mut outline_tools),
                    Err(e) => {
                        tracing::warn!(error = %e, "outline.list_tools_prefixed failed");
                    }
                }
            }
            if let Some(journal) = inner.journal.as_ref() {
                match journal.list_tools(request.clone(), context.clone()).await {
                    Ok(mut journal_tools) => tools.append(&mut journal_tools),
                    Err(e) => {
                        tracing::warn!(error = %e, "journal.list_tools failed");
                    }
                }
            }
        }
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
        // the session explicitly). `maybe_auto_start_session` keeps its write
        // guard in a tight scope, dropped before dispatch so that
        // try_plugin_call can acquire a read guard without deadlocking (R3).
        if request.name != "session_start" {
            maybe_auto_start_session(&self.state).await?;
        }

        // Export tools take priority over plugins: a route is a
        // session-bound, intentionally declared name, whereas a plugin's
        // name comes from whatever recipe files happen to be on disk.
        // Checked ahead of the `session_start` guard below for consistency,
        // even though export names never collide with `session_start`
        // itself.
        if request.name != "session_start"
            && let Some(result) = self
                .try_export_call(&request.name, request.arguments.as_ref())
                .await?
        {
            return Ok(result);
        }

        // Skip plugin lookup for `session_start` so it can recover after the
        // previous session's root has been removed. Without this guard,
        // RecipeModule::list_plugins (called inside try_plugin_call) hits the
        // K-239 check_session_root on the dead session and rejects session_start
        // itself — breaking the very recovery path the K-239 error message
        // promises. Mirrors the auto-start gate exemption above.
        if request.name != "session_start"
            && let Some(result) = self
                .try_plugin_call(&request.name, request.arguments.as_ref())
                .await?
        {
            return Ok(result);
        }
        // Outline delegation: if the tool name has the `outline_` prefix,
        // forward to the upstream OutlineMcpServer (which sees the raw
        // name with the prefix stripped).
        if request
            .name
            .starts_with(lds_outline::module::OUTLINE_TOOL_PREFIX)
        {
            let inner = self.state.read().await;
            if let Some(outline) = inner.outline.as_ref()
                && let Some(result) = outline.try_call(request.clone(), context.clone()).await?
            {
                return Ok(result);
            }
        }
        // Journal delegation: if the tool name has the `journal_` prefix,
        // forward to the upstream JournalMcpServer verbatim (journal tools
        // already carry the prefix in-crate, so no rewriting is needed).
        if request
            .name
            .starts_with(lds_journal::module::JOURNAL_TOOL_PREFIX)
        {
            let inner = self.state.read().await;
            if let Some(journal) = inner.journal.as_ref()
                && let Some(result) = journal.try_call(request.clone(), context.clone()).await?
            {
                return Ok(result);
            }
        }
        let tcc = ToolCallContext::new(self, request, context);
        Self::tool_router().call(tcc).await
    }

    // ── Resources ────────────────────────────────────────────────────────────
    //
    // Multi-session ledger introspection surfaced as MCP resources so any
    // resource-aware client can observe lds state without invoking tools.
    //
    //   lds://sessions              — full ledger (= session_list payload)
    //   lds://sessions/doctor       — doctor reports for every session
    //   lds://sessions/{key}        — single session description (id or alias)
    //   lds://sessions/{key}/doctor — doctor report for one session
    //   lds://docs/multi-session    — design / usage doc for the model
    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let inner = self.state.read().await;
        let mut resources: Vec<Resource> = vec![
            Annotated::new(
                RawResource::new("lds://sessions", "sessions")
                    .with_description("Full multi-session ledger as JSON")
                    .with_mime_type("application/json"),
                None,
            ),
            Annotated::new(
                RawResource::new("lds://sessions/doctor", "sessions/doctor")
                    .with_description("Doctor reports for every live session")
                    .with_mime_type("application/json"),
                None,
            ),
            Annotated::new(
                RawResource::new("lds://docs/multi-session", "docs/multi-session")
                    .with_description("Multi-session ledger design + usage doc")
                    .with_mime_type("text/markdown"),
                None,
            ),
            Annotated::new(
                RawResource::new("lds://docs/routing", "docs/routing")
                    .with_description(
                        "MCP routing + export: config.toml shape, tool contracts, usage examples",
                    )
                    .with_mime_type("text/markdown"),
                None,
            ),
        ];
        for entry in inner.lds.list_sessions() {
            let label = entry
                .alias
                .clone()
                .unwrap_or_else(|| entry.session_id.clone());
            resources.push(Annotated::new(
                RawResource::new(
                    format!("lds://sessions/{label}"),
                    format!("session/{label}"),
                )
                .with_description(format!("Session {label} (root={})", entry.root.display()))
                .with_mime_type("application/json"),
                None,
            ));
            resources.push(Annotated::new(
                RawResource::new(
                    format!("lds://sessions/{label}/doctor"),
                    format!("session/{label}/doctor"),
                )
                .with_description(format!("Doctor report for session {label}"))
                .with_mime_type("application/json"),
                None,
            ));
        }
        // Outline + Journal resources: forwarded verbatim from their upstream
        // MCP servers (outline:// / journal-mcp:// URI schemes are namespaced
        // by scheme, no rewriting needed).
        if let Some(outline) = inner.outline.as_ref() {
            match outline
                .list_resources(request.clone(), context.clone())
                .await
            {
                Ok(outline_resources) => resources.extend(outline_resources.resources),
                Err(e) => tracing::warn!(error = %e, "outline.list_resources failed"),
            }
        }
        if let Some(journal) = inner.journal.as_ref() {
            match journal.list_resources(request, context).await {
                Ok(journal_resources) => resources.extend(journal_resources.resources),
                Err(e) => tracing::warn!(error = %e, "journal.list_resources failed"),
            }
        }
        Ok(ListResourcesResult {
            resources,
            meta: None,
            next_cursor: None,
        })
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        let templates: Vec<ResourceTemplate> = vec![
            Annotated::new(
                RawResourceTemplate::new("lds://sessions/{key}", "session by key")
                    .with_description("Describe a session by id or alias")
                    .with_mime_type("application/json"),
                None,
            ),
            Annotated::new(
                RawResourceTemplate::new("lds://sessions/{key}/doctor", "session doctor by key")
                    .with_description("Doctor report for a single session")
                    .with_mime_type("application/json"),
                None,
            ),
        ];
        Ok(ListResourceTemplatesResult {
            resource_templates: templates,
            meta: None,
            next_cursor: None,
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        // Outline / Journal resources short-circuit lds's own resolver so
        // consumers can fetch bundled guides through lds.
        if request.uri.starts_with("outline://") {
            let inner = self.state.read().await;
            if let Some(outline) = inner.outline.as_ref()
                && let Some(result) = outline
                    .try_read_resource(request.clone(), context.clone())
                    .await?
            {
                return Ok(result);
            }
        }
        if request.uri.starts_with("journal-mcp://") {
            let inner = self.state.read().await;
            if let Some(journal) = inner.journal.as_ref()
                && let Some(result) = journal.try_read_resource(request.clone(), context).await?
            {
                return Ok(result);
            }
        }
        let uri = request.uri;
        let inner = self.state.read().await;
        let body = read_lds_resource(&uri, &inner.lds)?;
        Ok(ReadResourceResult::new(vec![body]))
    }
}

/// Resolve an `lds://` URI to a single resource body. Pure function so it can
/// be exercised by unit tests without spinning up a full MCP server.
fn read_lds_resource(uri: &str, ledger: &LdsState) -> Result<ResourceContents, McpError> {
    let path = uri
        .strip_prefix("lds://")
        .ok_or_else(|| McpError::invalid_params(format!("unknown URI scheme: {uri}"), None))?;

    if path == "docs/multi-session" {
        return Ok(ResourceContents::TextResourceContents {
            uri: uri.to_string(),
            mime_type: Some("text/markdown".into()),
            text: MULTI_SESSION_DOC.into(),
            meta: None,
        });
    }

    if path == "docs/routing" {
        return Ok(ResourceContents::TextResourceContents {
            uri: uri.to_string(),
            mime_type: Some("text/markdown".into()),
            text: ROUTING_DOC.into(),
            meta: None,
        });
    }

    if path == "sessions" {
        let entries: Vec<serde_json::Value> = ledger
            .list_sessions()
            .into_iter()
            .map(session_entry_to_json)
            .collect();
        let text = serde_json::to_string_pretty(&serde_json::json!({ "sessions": entries }))
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        return Ok(json_resource(uri, text));
    }

    if path == "sessions/doctor" {
        let mut reports: Vec<serde_json::Value> = Vec::new();
        for entry in ledger.list_sessions() {
            let r = ledger
                .doctor(&entry.session_id)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            reports.push(doctor_report_to_json(&r));
        }
        let text = serde_json::to_string_pretty(&serde_json::json!({ "reports": reports }))
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        return Ok(json_resource(uri, text));
    }

    if let Some(rest) = path.strip_prefix("sessions/") {
        let (key, want_doctor) = match rest.strip_suffix("/doctor") {
            Some(k) => (k, true),
            None => (rest, false),
        };
        if key.is_empty() || key.contains('/') {
            return Err(McpError::invalid_params(
                format!("malformed session URI: {uri}"),
                None,
            ));
        }
        if want_doctor {
            let r = ledger
                .doctor(key)
                .map_err(|e| McpError::resource_not_found(e.to_string(), None))?;
            let text = serde_json::to_string_pretty(&doctor_report_to_json(&r))
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(json_resource(uri, text));
        } else {
            let entry = ledger
                .describe(key)
                .map_err(|e| McpError::resource_not_found(e.to_string(), None))?;
            let text = serde_json::to_string_pretty(&session_entry_to_json(entry))
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(json_resource(uri, text));
        }
    }

    Err(McpError::resource_not_found(
        format!("unknown lds resource: {uri}"),
        None,
    ))
}

fn json_resource(uri: &str, text: String) -> ResourceContents {
    ResourceContents::TextResourceContents {
        uri: uri.to_string(),
        mime_type: Some("application/json".into()),
        text,
        meta: None,
    }
}

fn session_entry_to_json(e: lds_core::SessionEntry) -> serde_json::Value {
    serde_json::json!({
        "session_id": e.session_id,
        "alias": e.alias,
        "root": e.root.display().to_string(),
        "created_at": e.created_at,
        "last_used_at": e.last_used_at,
        "is_default": e.is_default,
    })
}

fn doctor_report_to_json(r: &lds_core::DoctorReport) -> serde_json::Value {
    let checks: Vec<serde_json::Value> = r
        .checks
        .iter()
        .map(|c| {
            serde_json::json!({
                "name": c.name,
                "status": c.status.as_str(),
                "evidence": c.evidence,
            })
        })
        .collect();
    serde_json::json!({
        "session_id": r.session_id,
        "alias": r.alias,
        "verdict": r.verdict.as_str(),
        "checks": checks,
    })
}

const MULTI_SESSION_DOC: &str = r#"# lds Multi-Session Ledger

lds tracks **multiple concurrent sessions** in an in-memory ledger so MainAI
and SubAgents (or fixture / worktree side-sessions) can coexist without one
silently overriding another's `root` binding.

## Addressing

Every session has two handles:

- `session_id` — opaque hash assigned at create time
- `alias`     — optional, human-readable label (`worker-1`, `fixture-A`, ...)

Pass either to `session_describe` / `session_doctor` / `session_close` /
`session_alias_set`. The legacy `session_start` tool always replaces the
**default session** for backward-compatible tool calls that omit
`session_id`.

## Resources

- `lds://sessions`              — full ledger as JSON
- `lds://sessions/doctor`       — doctor reports for every session
- `lds://sessions/{key}`        — describe one session (`key` = id or alias)
- `lds://sessions/{key}/doctor` — doctor report for one session
- `lds://docs/multi-session`    — this doc
- `lds://docs/routing`          — MCP routing + export design + usage

## Doctor checks (3-valued verdict: ok / warn / fail)

| check                 | what it verifies                                    |
|-----------------------|-----------------------------------------------------|
| root-exists           | the session root still exists on disk               |
| git-bound             | `.git` is present (git_* tools will work)           |
| journal-db-writable   | journal storage directory is writable               |
| stale-lock            | no leftover `.journal.db.lock` older than 1h        |
| ownership-drift       | no other session claims the same root               |
| root-conflict         | escalates to FAIL when ≥2 sessions share the root   |
| ledger-leak           | warns when a session has been idle > 6h             |

## Patterns

- **MainAI + worker SubAgent** — MainAI keeps the default session; each
  SubAgent calls `session_create` with its own root + alias.
- **Fixture / sandbox runs** — spawn an isolated session on a tempdir;
  close it when done.
- **Observability sweep** — periodically read `lds://sessions/doctor` to
  catch root drift, conflicts, and idle leakage.
"#;

const ROUTING_DOC: &str = r#"# lds MCP Routing + Export

lds proxies calls to **external MCP servers** via a session-bound gateway so
callers only need one tool surface (`mcp_call`) instead of loading every
upstream server's schema. Declared upstream tools can additionally be
**re-exported** to the caller's tool list with a prefixed name.

## Tools

- `mcp_call(uri, args)` — proxy a single call. `uri = "<route>://<tool>"`.
  `args` must be a JSON object.
- `mcp_route_list` — enumerate registered routes for the active session.
- `mcp_route_register(name, command, args?, env?, timeout_secs?)` — add or
  replace a route **in-memory only** (not persisted).
- `mcp_route_remove(name)` — remove a route and terminate its subprocess.
- `mcp_export_list` — enumerate currently materialized `[[export]]` tools.
- `mcp_export_refresh` — re-poll each declared export route's upstream
  `list_tools`, then rebuild the exported tool set atomically.

## Reserved scheme

`lds://` is reserved. `mcp_call(uri="lds://…")` returns
`RouterError::SelfLoop` before any lookup.

## Config file (`config.toml`)

Routes and exports live in the shared `config.toml` (same file as
`[recipes]` / `[paths]`). Two locations are merged at `session_start`:

- User-global: `~/.config/lds/config.toml`
- Project-local: `<session_root>/config.toml` (overrides user by `name` /
  `route`)

Both files are optional. Unknown top-level keys are ignored, so router
config coexists with existing `[recipes]` / `[paths]` sections without
schema conflict.

### `[[route]]`

```toml
[[route]]
name = "outline"
command = "outline-mcp"
args = ["--stdio"]
timeout_secs = 30                          # default 30
env = { OUTLINE_HOME = "${LDS_SESSION_ROOT}/.outline" }
```

Fields:

- `name` (required) — the `<route>` component of `<route>://<tool>` URIs.
  Must be unique per session.
- `command` (required) — subprocess command, resolved via `PATH`.
- `args` (default `[]`) — CLI arguments.
- `env` (default `{}`) — extra env vars for the subprocess.
- `timeout_secs` (default 30) — per-call timeout (applies to both
  `call_tool` and `list_tools`).

`${LDS_SESSION_ROOT}` is expanded in `args` and `env` values only.
`name` and `command` are never expanded — an unexpanded literal in
`command` will fail fast at `Command::new` time.

Subprocess spawn is **lazy**: no subprocess is started until the first
`mcp_call` (or `mcp_export_refresh`) touches the route.

### `[[export]]`

```toml
[[export]]
route = "outline"
tools = ["search_notes", "get_note"]
# prefix = "outline_"                     # default: "<route>_"
```

Fields:

- `route` (required) — must match a `[[route]]` `name` in the same
  session's config.
- `tools` (required) — the upstream tools' exact names, as an array.
- `prefix` (optional) — override the default `<route>_` prefix.

Declared exports appear in the caller's `list_tools` as
`<prefix><tool>` (e.g. `outline_search_notes`), with the upstream tool's
schema copied verbatim. Undeclared upstream tools remain accessible only
via generic `mcp_call`.

**Limits**:

- Total exported tool count is capped at 16 by default. Exceeding it
  fails `session_start` with `RouterError::ExportLimitExceeded`.
- Name collisions between exports (same prefixed name) fail
  `session_start` with `RouterError::ExportCollision`.
- Collisions with lds's own static tool names (e.g. `git_status`,
  `session_start`) also fail — the router keeps the built-in and rejects
  the export.

## Usage examples

### Proxy a single call

```
mcp_call(uri="outline://search_notes", args={"query": "changelog"})
  → JSON returned verbatim from outline-mcp
```

### Auto-register at session start (persistent)

Add `[[route]]` blocks to `~/.config/lds/config.toml` — they are wired
automatically the next time `session_start` runs (or on the first
tool call that triggers auto-start).

### Runtime-only route (not persisted)

```
mcp_route_register(name="scratch", command="my-experimental-mcp")
mcp_call(uri="scratch://ping")
mcp_route_remove(name="scratch")
```

### Refresh export schemas after an upstream restart

```
mcp_export_refresh                 # atomic rebuild; export snapshot swaps in one step
mcp_export_list                    # confirm the new tool surface
```

## Failure modes

| Error                       | When                                                       |
|-----------------------------|------------------------------------------------------------|
| `RouterError::SelfLoop`     | `mcp_call(uri="lds://…")` — reserved scheme rejected       |
| `RouterError::InvalidUri`   | URI does not match `<route>://<tool>`                      |
| `RouterError::RouteNotFound`| No route registered for the URI's `<route>`                |
| `RouterError::Timeout`      | Upstream `call_tool` or `list_tools` exceeded `timeout_secs` |
| `RouterError::Spawn`        | Subprocess spawn failed (typically `command` not on `PATH`) |
| `RouterError::Upstream`     | Transparent upstream MCP-level error (message forwarded)    |
| `RouterError::Config`       | `config.toml` `[[route]]` / `[[export]]` parse error       |
| `RouterError::ExportLimitExceeded` | `[[export]]` total exceeds 16                       |
| `RouterError::ExportCollision`     | Two exports produce the same prefixed tool name     |

## Concurrency

- `mcp_call` acquires the router's read lock only for HashMap lookup +
  `Arc<RouteClient>` clone; the upstream `.await` runs lock-free.
- Calls to the *same* route serialize on a per-`RouteClient` mutex (one
  stdio stream per subprocess). Calls to *different* routes are fully
  concurrent.
- `session_start` / auto-start hold the session write lock only for the
  synchronous local module wiring. Network I/O (`list_tools` for each
  declared export) runs after the lock is dropped; the write lock is
  reacquired only long enough to assign the built router + export
  registry.

## See also

- Tool doc: use MCP `tools/list` — each `mcp_*` tool carries its own
  `description` + `annotations` (idempotent / destructive hints).
- Resource: `lds://docs/multi-session` — session model this router
  builds on top of.
"#;

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
///
/// The session (and its routes/exports, if any) is eagerly auto-started
/// here — before the transport starts serving — so that a client's very
/// first `tools/list` request (issued immediately after the `initialize`
/// handshake) already observes materialized `[[export]]` tools instead of
/// racing session construction. Without this, `list_tools` only reflects
/// exports after some tool call has triggered `call_tool`'s own auto-start
/// hook, which for a fresh session is typically *after* the client has
/// already cached an export-less tool list.
async fn serve_mcp() -> Result<()> {
    tracing::info!("lds v{}", env!("CARGO_PKG_VERSION"));
    let server = LdsServer::new();
    maybe_auto_start_session(&server.state).await?;
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
            ..Default::default()
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
            ..Default::default()
        };
        let dirs = resolve_startup_global_dirs(cfg, None);
        assert_eq!(dirs, vec![PathBuf::from("/config/only")]);
    }

    #[test]
    fn parse_since_string_accepts_epoch_integer() {
        assert_eq!(parse_since_string("1751932800").unwrap(), 1751932800);
        assert_eq!(parse_since_string("0").unwrap(), 0);
        assert_eq!(parse_since_string("  42  ").unwrap(), 42);
    }

    #[test]
    fn parse_since_string_accepts_rfc3339_utc() {
        // 2026-07-14T00:00:00Z.
        let epoch = parse_since_string("2026-07-14T00:00:00Z").unwrap();
        assert_eq!(epoch, 1_783_987_200);
    }

    #[test]
    fn parse_since_string_accepts_rfc3339_with_offset() {
        // 2026-07-14T09:00:00+09:00 == 2026-07-14T00:00:00Z.
        let with_offset = parse_since_string("2026-07-14T09:00:00+09:00").unwrap();
        let utc = parse_since_string("2026-07-14T00:00:00Z").unwrap();
        assert_eq!(with_offset, utc);
    }

    #[test]
    fn parse_since_string_rejects_garbage() {
        assert!(parse_since_string("yesterday").is_err());
        assert!(parse_since_string("2026-99-99T00:00:00Z").is_err());
    }
}
