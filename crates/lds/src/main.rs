mod cli;

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use lds_core::config::Config;
use lds_core::{LdsState, Session, SessionConfig, check_binaries};
use lds_gh::GhModule;
use lds_git::{GitModule, ResetMode};
use lds_journal::JournalModule;
use lds_journal::journal_mcp_core;
use lds_journal::tools::{
    ChapterListRow, JournalAppendProgressParams, JournalAppendSectionParams,
    JournalChapterListParams, JournalCloseChapterParams, JournalGrepParams, JournalImportParams,
    JournalInfoParams, JournalInfoResult, JournalOpenChapterParams, JournalOpenChaptersParams,
    JournalProgressOfParams, JournalProjectionAttachParams, JournalProjectionDetachParams,
    JournalProjectionRebuildParams, JournalSchemaListParams, JournalSchemaLoadParams,
    JournalSchemaShowParams, JournalTailParams, chapter_replay_to_json, paginate,
};
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
    recipe: Option<RecipeModule>,
    sandbox_fs: Option<SandboxFs>,
    sandbox_python: Option<SandboxPython>,
    journal: Option<JournalModule>,
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
                recipe: None,
                sandbox_fs: None,
                sandbox_python: None,
                journal: None,
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
    inner.recipe = Some(RecipeModule::new(Arc::clone(&session)));
    inner.sandbox_fs = Some(
        SandboxFs::new(session.root())
            .map_err(|e| McpError::internal_error(e.to_string(), None))?,
    );
    inner.sandbox_python = Some(SandboxPython::new(session.root()));
    inner.journal = Some({
        let module = JournalModule::new(Arc::clone(&session))
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        // Opt-in file projection: LDS_JOURNAL_FILE_ENABLE turns it on; the
        // path defaults to <root>/workspace/journal.md but can be overridden
        // via LDS_JOURNAL_FILE_OUTPUT_PATH (relative paths resolve against
        // session.root()).
        if std::env::var_os("LDS_JOURNAL_FILE_ENABLE").is_some() {
            match std::env::var_os("LDS_JOURNAL_FILE_OUTPUT_PATH") {
                Some(raw) => {
                    let p = PathBuf::from(raw);
                    let resolved = if p.is_absolute() {
                        p
                    } else {
                        session.root().join(p)
                    };
                    module
                        .enable_file_projection_at(resolved)
                        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                }
                None => {
                    module
                        .enable_file_projection()
                        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                }
            }
        }
        module
    });
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
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
    }

    #[tool(description = "Show git commit log as JSON ({commits: [{sha, short_sha, summary}]})")]
    async fn git_log(
        &self,
        Parameters(req): Parameters<GitLogReq>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner.git.as_ref().ok_or_else(no_session_error)?;
        let out = git
            .log(req.max_count)
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
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
    }

    #[tool(description = "List git worktrees as JSON, each with session ownership flag")]
    async fn git_worktree_list(&self) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner.git.as_ref().ok_or_else(no_session_error)?;
        let out = git
            .worktree_list()
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
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
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
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
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
    }

    #[tool(description = "List git remotes as JSON ({remotes: [{name, fetch_url, push_url}]})")]
    async fn git_remote_list(&self) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let git = inner.git.as_ref().ok_or_else(no_session_error)?;
        let out = git
            .remote_list()
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
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
    }

    #[tool(
        description = "Reset HEAD in a session-owned working directory. mode = soft | mixed | hard"
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
            .reset(&working_dir, mode, &req.target)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&out)
    }

    #[tool(
        description = "Adopt orphan worktrees under .worktrees/ left behind by a previous session"
    )]
    async fn git_session_release(&self) -> Result<CallToolResult, McpError> {
        let mut inner = self.state.write().await;
        let git = inner.git.as_mut().ok_or_else(no_session_error)?;
        let out = git
            .session_release()
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

    // -----------------------------------------------------------------------
    // Journal — 17 tools ported from journal-mcp v0.4.0 (crates.io: journal-mcp-core 0.4.0)
    //
    // session.root() is the SoT for all path resolution: per-call
    // project_root overrides are gone. file projection (the
    // <root>/workspace/journal.md rendering) is opt-in via:
    //   (a) LDS_JOURNAL_FILE_ENABLE env at session start, or
    //   (b) journal_projection_attach { name="file", output_path? } MCP call.
    // -----------------------------------------------------------------------

    #[tool(
        description = "Open a new journal chapter (name + schema_id) and return its chapter ID."
    )]
    async fn journal_open_chapter(
        &self,
        Parameters(req): Parameters<JournalOpenChapterParams>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let journal = inner.journal.as_ref().ok_or_else(no_session_error)?;
        let id = journal
            .lock_core()
            .open_chapter(&req.name, &req.schema_id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&serde_json::json!({ "chapter_id": id.0 }))
    }

    #[tool(description = "Append a section row to an open chapter. \
                       Returns {\"warnings\": [{kind, section, hint}, ...]} (may be empty).")]
    async fn journal_append_section(
        &self,
        Parameters(req): Parameters<JournalAppendSectionParams>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let journal = inner.journal.as_ref().ok_or_else(no_session_error)?;
        let chapter_id = journal_mcp_core::ChapterId(req.chapter_id);
        let warnings = journal
            .lock_core()
            .append_section(&chapter_id, &req.section_name, &req.body)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let warning_objs: Vec<serde_json::Value> = warnings
            .iter()
            .map(|w| {
                serde_json::json!({
                    "kind": w.kind,
                    "section": w.section,
                    "hint": w.hint,
                })
            })
            .collect();
        json_result(&serde_json::json!({ "warnings": warning_objs }))
    }

    #[tool(
        description = "Append a single line to the 'Progress' section of an open chapter. \
                       Returns {\"warnings\": [...]} (may be empty)."
    )]
    async fn journal_append_progress(
        &self,
        Parameters(req): Parameters<JournalAppendProgressParams>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let journal = inner.journal.as_ref().ok_or_else(no_session_error)?;
        let chapter_id = journal_mcp_core::ChapterId(req.chapter_id);
        let warnings = journal
            .lock_core()
            .append_progress(&chapter_id, &req.line)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let warning_objs: Vec<serde_json::Value> = warnings
            .iter()
            .map(|w| {
                serde_json::json!({
                    "kind": w.kind,
                    "section": w.section,
                    "hint": w.hint,
                })
            })
            .collect();
        json_result(&serde_json::json!({ "warnings": warning_objs }))
    }

    #[tool(description = "Close an open chapter. \
                       Validates all schema requires preconditions before writing. \
                       Returns {\"ok\": true} on success.")]
    async fn journal_close_chapter(
        &self,
        Parameters(req): Parameters<JournalCloseChapterParams>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let journal = inner.journal.as_ref().ok_or_else(no_session_error)?;
        let chapter_id = journal_mcp_core::ChapterId(req.chapter_id);
        journal
            .lock_core()
            .close_chapter(&chapter_id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&serde_json::json!({ "ok": true }))
    }

    #[tool(
        description = "Load a ChapterSchema YAML literal into the SchemaRegistry L2 layer. \
                       Returns the registry key that was inserted (e.g. \"journal-mcp-canonical-v1\"). \
                       Idempotent: repeated calls with the same YAML overwrite with the same value."
    )]
    async fn journal_schema_load(
        &self,
        Parameters(req): Parameters<JournalSchemaLoadParams>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let journal = inner.journal.as_ref().ok_or_else(no_session_error)?;
        let key = journal
            .lock_core()
            .load_schema_yaml(&req.yaml)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&serde_json::json!({ "key": key }))
    }

    #[tool(
        description = "List all available schema registry keys (built-in L1 + project-local L2). \
                       Returns {\"keys\": [\"...\", ...]}."
    )]
    async fn journal_schema_list(
        &self,
        Parameters(_req): Parameters<JournalSchemaListParams>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let journal = inner.journal.as_ref().ok_or_else(no_session_error)?;
        let keys = journal.lock_core().schema_keys();
        json_result(&serde_json::json!({ "keys": keys }))
    }

    #[tool(
        description = "Show the specification of a given schema registry key as a JSON object. \
                       Returns an error if the key is not found."
    )]
    async fn journal_schema_show(
        &self,
        Parameters(req): Parameters<JournalSchemaShowParams>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let journal = inner.journal.as_ref().ok_or_else(no_session_error)?;
        let core = journal.lock_core();
        let spec = core.schema_spec(&req.key).ok_or_else(|| {
            McpError::invalid_params(format!("schema not found: {}", req.key), None)
        })?;
        let sections: serde_json::Map<String, serde_json::Value> = spec
            .sections()
            .iter()
            .map(|(k, v)| {
                let section_json = serde_json::json!({
                    "required": v.required,
                    "evidence_required": v.evidence_required,
                    "description": v.description,
                });
                (k.clone(), section_json)
            })
            .collect();
        let states: Vec<serde_json::Value> = spec
            .states()
            .iter()
            .map(|s| {
                serde_json::json!({
                    "id": s.id,
                    "initial": s.initial,
                    "terminal": s.terminal,
                })
            })
            .collect();
        let transitions: Vec<serde_json::Value> = spec
            .transitions()
            .iter()
            .map(|t| serde_json::json!({ "from": t.from, "to": t.to, "on": t.on }))
            .collect();
        let val = serde_json::json!({
            "schema_id": spec.schema_id(),
            "version": spec.version(),
            "states": states,
            "transitions": transitions,
            "sections": sections,
            "section_order": spec.section_order(),
            "chapter_header": spec.chapter_header(),
            "section_header": spec.section_header(),
        });
        json_result(&val)
    }

    #[tool(description = "Fetch the last N chapters (default 10), newest first. \
                       Returns a JSON array of chapter objects with metadata and events.")]
    async fn journal_tail(
        &self,
        Parameters(req): Parameters<JournalTailParams>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let journal = inner.journal.as_ref().ok_or_else(no_session_error)?;
        let n = req.n.unwrap_or(10);
        let chapters = journal
            .lock_core()
            .tail_chapters(n)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let values: Vec<serde_json::Value> = chapters.iter().map(chapter_replay_to_json).collect();
        json_result(&values)
    }

    #[tool(
        description = "Search all chapter section bodies for a substring pattern. \
                       Optional since/until (Unix epoch ms) filter which chapters are scanned \
                       by their opened_at timestamp. \
                       Returns a JSON array of {chapter_id, section_name, body} matches."
    )]
    async fn journal_grep(
        &self,
        Parameters(req): Parameters<JournalGrepParams>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let journal = inner.journal.as_ref().ok_or_else(no_session_error)?;
        let core = journal.lock_core();

        let since_ids = core
            .chapter_ids(req.since)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let allowed: std::collections::HashSet<String> = if let Some(until_ms) = req.until {
            let all = core
                .tail_chapters(usize::MAX)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            let since_set: std::collections::HashSet<String> =
                since_ids.iter().map(|id| id.0.clone()).collect();
            all.into_iter()
                .filter(|r| {
                    r.meta.opened_at <= until_ms && since_set.contains(&r.meta.chapter_id.0)
                })
                .map(|r| r.meta.chapter_id.0)
                .collect()
        } else {
            since_ids.into_iter().map(|id| id.0).collect()
        };

        let raw = core
            .grep_chapters(&req.pattern)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let matches: Vec<serde_json::Value> = raw
            .into_iter()
            .filter(|(cid, _, _)| allowed.contains(&cid.0))
            .map(|(cid, section_name, body)| {
                serde_json::json!({
                    "chapter_id": cid.0,
                    "section_name": section_name,
                    "body": body,
                })
            })
            .collect();
        json_result(&matches)
    }

    #[tool(
        description = "List all chapters as a summary table (Microsoft Decision Log format). \
                       Returns a JSON array with chapter_id, schema_id, current_state, \
                       opened_at, closed_at, decided_summary, and link fields."
    )]
    async fn journal_chapter_list(
        &self,
        Parameters(req): Parameters<JournalChapterListParams>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let journal = inner.journal.as_ref().ok_or_else(no_session_error)?;
        let chapters = journal
            .lock_core()
            .tail_chapters(usize::MAX)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let paginated = paginate(chapters, req.offset, req.limit);
        let rows: Vec<ChapterListRow> = paginated
            .into_iter()
            .map(|replay| {
                let decided_summary = replay
                    .events
                    .iter()
                    .filter(|e| {
                        e.event_type == "section_append"
                            && e.section_name.as_deref() == Some("Decided")
                    })
                    .find_map(|e| {
                        serde_json::from_str::<serde_json::Value>(&e.payload)
                            .ok()
                            .and_then(|v| {
                                v.get("body")
                                    .and_then(|b| b.as_str())
                                    .map(|s| s.lines().next().unwrap_or("").to_owned())
                            })
                    })
                    .unwrap_or_default();
                let link = format!("{}#decided", replay.meta.chapter_id.0);
                ChapterListRow {
                    chapter_id: replay.meta.chapter_id.0.clone(),
                    schema_id: replay.meta.schema_id.clone(),
                    current_state: replay.meta.current_state.clone(),
                    opened_at: replay.meta.opened_at,
                    closed_at: replay.meta.closed_at,
                    decided_summary,
                    link,
                }
            })
            .collect();
        json_result(&rows)
    }

    #[tool(
        description = "List all chapters that are still open (closed_at IS NULL). \
                       Returns {\"open_chapter_ids\": [\"...\", ...]}."
    )]
    async fn journal_open_chapters(
        &self,
        Parameters(_req): Parameters<JournalOpenChaptersParams>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let journal = inner.journal.as_ref().ok_or_else(no_session_error)?;
        let ids = journal
            .lock_core()
            .open_chapter_ids()
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let id_strs: Vec<String> = ids.into_iter().map(|id| id.0).collect();
        json_result(&serde_json::json!({ "open_chapter_ids": id_strs }))
    }

    #[tool(description = "Read the Progress section of a specific chapter. \
                       Returns {\"progress\": [\"...\", ...]} in append order.")]
    async fn journal_progress_of(
        &self,
        Parameters(req): Parameters<JournalProgressOfParams>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let journal = inner.journal.as_ref().ok_or_else(no_session_error)?;
        let chapter_id = journal_mcp_core::ChapterId(req.chapter_id);
        let entries = journal
            .lock_core()
            .progress_of(&chapter_id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&serde_json::json!({ "progress": entries }))
    }

    #[tool(
        description = "Attach a named projection at runtime. Currently only 'file' is supported. \
                       output_path (optional): relative paths resolve against session.root(), \
                       absolute paths are used as-is. When omitted, falls back to \
                       LDS_JOURNAL_FILE_OUTPUT_PATH env or <root>/workspace/journal.md. \
                       Other names return an error."
    )]
    async fn journal_projection_attach(
        &self,
        Parameters(req): Parameters<JournalProjectionAttachParams>,
    ) -> Result<CallToolResult, McpError> {
        if req.name != "file" {
            return Err(McpError::invalid_params(
                format!("projection not found: {}", req.name),
                None,
            ));
        }
        let inner = self.state.read().await;
        let journal = inner.journal.as_ref().ok_or_else(no_session_error)?;
        let session_root = journal.session().root().to_path_buf();
        let resolved = if let Some(raw) = req.output_path {
            let p = PathBuf::from(raw);
            if p.is_absolute() {
                p
            } else {
                session_root.join(p)
            }
        } else if let Some(env) = std::env::var_os("LDS_JOURNAL_FILE_OUTPUT_PATH") {
            let p = PathBuf::from(env);
            if p.is_absolute() {
                p
            } else {
                session_root.join(p)
            }
        } else {
            session_root.join("workspace").join("journal.md")
        };
        journal
            .enable_file_projection_at(resolved.clone())
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&serde_json::json!({
            "attached": "file",
            "output_path": resolved,
        }))
    }

    #[tool(description = "Detach a named projection. \
                       NOT YET SUPPORTED in this release (carry from journal-mcp v0.4.0). \
                       Always returns an error.")]
    async fn journal_projection_detach(
        &self,
        Parameters(_req): Parameters<JournalProjectionDetachParams>,
    ) -> Result<CallToolResult, McpError> {
        Err(McpError::invalid_params(
            "projection detach is not yet supported (carry from journal-mcp v0.4.0)",
            None,
        ))
    }

    #[tool(
        description = "Rebuild a named projection by replaying the full EventLog. \
                       Calls rebuild_chapter for every closed chapter. \
                       Use 'file' to rebuild the journal.md output. \
                       Optional output_path overrides the default attached path for a one-shot \
                       rebuild (file projection only; attached projection is unchanged)."
    )]
    async fn journal_projection_rebuild(
        &self,
        Parameters(req): Parameters<JournalProjectionRebuildParams>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let journal = inner.journal.as_ref().ok_or_else(no_session_error)?;
        let effective_root = journal.session().root().to_path_buf();

        if let Some(raw_output) = req.output_path.as_ref() {
            if req.name == "file" {
                let output_path = {
                    let p = PathBuf::from(raw_output);
                    if p.is_absolute() {
                        p
                    } else {
                        effective_root.join(&p)
                    }
                };
                if let Some(parent) = output_path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        McpError::internal_error(
                            format!("failed to create output_path parent dir: {e}"),
                            None,
                        )
                    })?;
                }
                let registry = journal_mcp_core::SchemaRegistry::with_project_local(
                    &effective_root,
                )
                .map_err(|e| {
                    McpError::internal_error(
                        format!("SchemaRegistry::with_project_local: {e}"),
                        None,
                    )
                })?;
                let registry_arc = Arc::new(registry);
                let mut temp_proj =
                    journal_mcp_core::FileProjection::new(output_path.clone(), registry_arc);
                let all_chapters = journal
                    .lock_core()
                    .tail_chapters(usize::MAX)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                for replay in &all_chapters {
                    if replay.meta.closed_at.is_none() {
                        continue;
                    }
                    use journal_mcp_core::JournalProjection as _;
                    temp_proj
                        .rebuild_chapter(replay)
                        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                }
                return json_result(&serde_json::json!({
                    "projection": "file",
                    "mode": "one-shot",
                    "output_path": output_path,
                }));
            } else {
                tracing::warn!(
                    name = %req.name,
                    "journal_projection_rebuild: output_path is only applicable for name='file'; \
                     ignoring and using default rebuild"
                );
            }
        }

        journal
            .lock_core()
            .rebuild_projection(&req.name)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        json_result(&serde_json::json!({
            "projection": req.name,
            "mode": "default",
        }))
    }

    #[tool(
        description = "Import chapters from a markdown file (journal-mcp-canonical-v1: h2=chapter, h3=section). \
                       Relative paths resolve against session.root(). \
                       Atomic batch insert — any chapter_id collision rolls back the entire batch. \
                       Returns {\"imported_chapter_ids\": [\"...\", ...]}. \
                       Does NOT trigger projection rebuild (call journal_projection_rebuild explicitly)."
    )]
    async fn journal_import(
        &self,
        Parameters(req): Parameters<JournalImportParams>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let journal = inner.journal.as_ref().ok_or_else(no_session_error)?;
        let session_root = journal.session().root().to_path_buf();
        let raw_path = PathBuf::from(&req.path);
        let resolved = if raw_path.is_absolute() {
            raw_path
        } else {
            session_root.join(raw_path)
        };
        let imported = journal
            .lock_core()
            .import_chapter(&resolved)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let ids: Vec<String> = imported.into_iter().map(|id| id.0).collect();
        json_result(&serde_json::json!({ "imported_chapter_ids": ids }))
    }

    #[tool(
        description = "Return journal runtime state (paths, schemas, version, file projection status). \
                       Read-only diagnostic tool; no side effects."
    )]
    async fn journal_info(
        &self,
        Parameters(_req): Parameters<JournalInfoParams>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.state.read().await;
        let journal = inner.journal.as_ref().ok_or_else(no_session_error)?;
        let session_root = journal.session().root().to_path_buf();
        let db_path = session_root.join("workspace").join(".journal.db");
        let db_exists = db_path.exists();
        let wal_path = {
            let mut p = db_path.clone().into_os_string();
            p.push("-wal");
            PathBuf::from(p)
        };
        let shm_path = {
            let mut p = db_path.clone().into_os_string();
            p.push("-shm");
            PathBuf::from(p)
        };
        let schema_registry_path = session_root.join(".journal").join("schemas");
        let available_schemas = journal.lock_core().schema_keys();
        let file_projection_path = journal.file_projection_path();
        let file_projection_enabled = file_projection_path.is_some();
        let result = JournalInfoResult {
            project_root: session_root,
            db_path,
            db_exists,
            wal_path,
            shm_path,
            schema_registry_path,
            available_schemas,
            version: env!("CARGO_PKG_VERSION").to_string(),
            file_projection_enabled,
            file_projection_path,
        };
        json_result(&result)
    }
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
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let mut tools = Self::tool_router().list_all();
        let plugins = self.list_plugin_tools().await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "list_plugin_tools failed during list_tools");
            Vec::new()
        });
        tools.extend(plugins);
        tools.extend(self.list_export_tools().await);
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
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
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
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
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
