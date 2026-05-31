//! Justfile recipe execution with hierarchical resolve chain.
//!
//! Discovers justfiles at two levels — Global (`~/.config/lds/`) and
//! Project (`{root}/`) — merges their recipes (project wins on name
//! collision), and tags each recipe with [`ResolveInfo`] so callers
//! know which justfile it came from. Adding a level (e.g. Worktree)
//! is one enum variant + one path probe.
//!
//! Execution applies [`Session`] timeout and output truncation, and
//! injects `content` args as `TASK_MCP_CONTENT_{KEY}` environment
//! variables for the recipe process.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use lds_core::log_store::{HasId, LogStore};
use lds_core::{Session, truncate_output};
use serde::Deserialize;

const ALLOW_AGENT_GROUP: &str = "allow-agent";
const LEGACY_ALLOW_AGENT_DOC: &str = "[allow-agent]";
const PLUGIN_GROUP: &str = "lds-plugin";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveLevel {
    Global,
    Project,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ResolveInfo {
    pub level: ResolveLevel,
    pub source_path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum RecipeError {
    #[error("recipe not found: {0}")]
    NotFound(String),
    #[error("invalid content key '{0}': must match [A-Za-z][A-Za-z0-9_]*")]
    InvalidContentKey(String),
    #[error("dangerous argument value: contains control character")]
    DangerousArgument,
    #[error("recipe '{0}' timed out after {1}s")]
    Timeout(String, u64),
    #[error("recipe '{0}' not in allow-agent group")]
    NotAllowed(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("just --dump failed for {}: {stderr}", justfile.display())]
    JustDumpFailed { justfile: PathBuf, stderr: String },
    #[error("just --dump JSON parse failed for {}", justfile.display())]
    JustDumpParse {
        justfile: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(transparent)]
    Session(#[from] lds_core::SessionError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecipeMode {
    #[default]
    AgentOnly,
    Unrestricted,
}

const LOG_CAPACITY: usize = 10;

#[derive(Debug)]
pub struct RecipeModule {
    session: Arc<Session>,
    resolve_chain: Vec<(ResolveLevel, PathBuf)>,
    mode: RecipeMode,
    log_store: LogStore<RecipeOutput>,
}

impl RecipeModule {
    pub fn new(session: Arc<Session>) -> Self {
        Self::with_mode(session, RecipeMode::default())
    }

    pub fn with_mode(session: Arc<Session>, mode: RecipeMode) -> Self {
        let resolve_chain = build_resolve_chain(session.root(), session.global_recipe_dirs());
        for (level, path) in &resolve_chain {
            tracing::info!(level = ?level, justfile = %path.display(), "recipe module: justfile found");
        }
        if resolve_chain.is_empty() {
            tracing::warn!("recipe module: no justfile found");
        }
        Self {
            session,
            resolve_chain,
            mode,
            log_store: LogStore::new(LOG_CAPACITY),
        }
    }

    pub fn logs(&self) -> &LogStore<RecipeOutput> {
        &self.log_store
    }

    pub fn resolve_chain(&self) -> &[(ResolveLevel, PathBuf)] {
        &self.resolve_chain
    }

    pub async fn list_plugins(&self) -> Result<Vec<PluginRecipe>> {
        self.session
            .ensure_alive()
            .map_err(|e| anyhow::anyhow!(e))?;
        let mut merged: HashMap<String, PluginRecipe> = HashMap::new();
        for (level, justfile_path) in &self.resolve_chain {
            let recipes = dump_justfile(justfile_path)
                .await
                .map_err(anyhow::Error::from)?;
            for r in recipes {
                if r.private {
                    continue;
                }
                if !is_plugin(&r) {
                    continue;
                }
                let parameters: Vec<PluginParam> = r
                    .parameters
                    .into_iter()
                    .map(|p| PluginParam {
                        name: p.name,
                        default: p.default,
                    })
                    .collect();
                merged.insert(
                    r.name.clone(),
                    PluginRecipe {
                        name: r.name,
                        description: r.doc.unwrap_or_default(),
                        parameters,
                        source: ResolveInfo {
                            level: *level,
                            source_path: justfile_path.clone(),
                        },
                    },
                );
            }
        }
        let mut result: Vec<PluginRecipe> = merged.into_values().collect();
        result.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(result)
    }

    pub async fn list(&self) -> Result<Vec<RecipeInfo>> {
        self.session
            .ensure_alive()
            .map_err(|e| anyhow::anyhow!(e))?;
        self.merged_recipes().await.map_err(anyhow::Error::from)
    }

    pub async fn run(
        &self,
        recipe: &str,
        args: &[&str],
        content: &HashMap<String, String>,
        timeout_override: Option<u64>,
    ) -> Result<RecipeOutput, RecipeError> {
        self.session.ensure_alive()?;
        for arg in args {
            validate_arg_value(arg)?;
        }
        for key in content.keys() {
            validate_content_key(key)?;
        }

        let resolved = self.merged_recipes().await?;
        let recipe_info = resolved.iter().find(|r| r.name == recipe);

        if self.mode == RecipeMode::AgentOnly && recipe_info.is_none() {
            return Err(RecipeError::NotAllowed(recipe.to_string()));
        }

        let source_path = recipe_info
            .map(|r| r.source.source_path.clone())
            .ok_or_else(|| RecipeError::NotFound(recipe.to_string()))?;

        let mut cmd = tokio::process::Command::new("just");
        cmd.arg("--justfile")
            .arg(&source_path)
            .arg("--working-directory")
            .arg(self.session.root())
            .arg(recipe);
        for arg in args {
            cmd.arg(arg);
        }

        for (key, value) in content {
            let env_key = format!("TASK_MCP_CONTENT_{}", key.to_uppercase());
            cmd.env(&env_key, value);
        }

        let timeout_secs = timeout_override.unwrap_or(self.session.timeout().as_secs());
        let timeout = std::time::Duration::from_secs(timeout_secs);
        let started_at = now_epoch();
        let result = tokio::time::timeout(timeout, cmd.output()).await;
        let duration_ms = now_epoch().saturating_sub(started_at) * 1000;
        let exec_id = exec_uuid();

        let output = match result {
            Ok(Ok(output)) => {
                let max = self.session.max_output();
                let (stdout, stdout_trunc) = truncate_output(&output.stdout, max);
                let (stderr, stderr_trunc) = truncate_output(&output.stderr, max);
                RecipeOutput {
                    id: exec_id,
                    started_at,
                    duration_ms,
                    stdout,
                    stderr,
                    exit_code: output.status.code().unwrap_or(-1),
                    timed_out: false,
                    truncated: stdout_trunc || stderr_trunc,
                }
            }
            Ok(Err(e)) => return Err(RecipeError::Io(e)),
            Err(_) => RecipeOutput {
                id: exec_id,
                started_at,
                duration_ms,
                stdout: String::new(),
                stderr: format!("recipe '{}' timed out after {}s", recipe, timeout_secs),
                exit_code: -1,
                timed_out: true,
                truncated: false,
            },
        };
        self.log_store.push(output.clone());
        Ok(output)
    }

    async fn merged_recipes(&self) -> Result<Vec<RecipeInfo>, RecipeError> {
        let mut merged: HashMap<String, RecipeInfo> = HashMap::new();

        // Iterate from lowest priority (Global) to highest (Project).
        // Higher priority overwrites on name collision.
        for (level, justfile_path) in &self.resolve_chain {
            let recipes = dump_justfile(justfile_path).await?;
            for r in recipes {
                if r.private {
                    continue;
                }
                if self.mode == RecipeMode::AgentOnly && !is_allow_agent(&r) && !is_plugin(&r) {
                    continue;
                }
                merged.insert(
                    r.name.clone(),
                    RecipeInfo {
                        name: r.name,
                        description: r.doc.unwrap_or_default(),
                        parameters: r.parameters.into_iter().map(|p| p.name).collect(),
                        source: ResolveInfo {
                            level: *level,
                            source_path: justfile_path.clone(),
                        },
                    },
                );
            }
        }

        let mut result: Vec<RecipeInfo> = merged.into_values().collect();
        result.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(result)
    }
}

/// Scan global plugin recipes from all global dirs, in precedence order.
///
/// Scans `~/.config/lds/justfile` first (default), then each entry in
/// `global_dirs` in slice order. Later entries override earlier ones on name
/// collision (same behaviour as `build_resolve_chain`). Used at server startup
/// to expose plugin tools before `session_start` is called.
pub async fn list_global_plugins(global_dirs: &[PathBuf]) -> Result<Vec<PluginRecipe>> {
    // Build the ordered list of dirs to scan: default first, then extra dirs.
    let mut dirs_to_scan: Vec<PathBuf> = Vec::new();
    if let Some(home) = home_dir() {
        dirs_to_scan.push(home.join(".config/lds"));
    }
    dirs_to_scan.extend_from_slice(global_dirs);

    let mut merged: HashMap<String, PluginRecipe> = HashMap::new();
    for dir in &dirs_to_scan {
        let Some(path) = find_justfile(dir) else {
            continue;
        };
        let recipes = dump_justfile(&path).await.map_err(anyhow::Error::from)?;
        for r in recipes {
            if r.private || !is_plugin(&r) {
                continue;
            }
            let parameters: Vec<PluginParam> = r
                .parameters
                .into_iter()
                .map(|p| PluginParam {
                    name: p.name,
                    default: p.default,
                })
                .collect();
            // Later dir overrides earlier on collision (higher priority).
            merged.insert(
                r.name.clone(),
                PluginRecipe {
                    name: r.name,
                    description: r.doc.unwrap_or_default(),
                    parameters,
                    source: ResolveInfo {
                        level: ResolveLevel::Global,
                        source_path: path.clone(),
                    },
                },
            );
        }
    }
    let mut plugins: Vec<PluginRecipe> = merged.into_values().collect();
    plugins.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(plugins)
}

/// Build the justfile resolve chain for a session.
///
/// Push order (lowest → highest priority):
/// 1. Default `~/.config/lds` — always consulted first.
/// 2. Each entry in `global_dirs` in slice order (crux §2: ENV-listed order preserved).
/// 3. Project `<root>/justfile` — highest priority.
///
/// Only directories where `find_justfile` succeeds are pushed. Callers arrange
/// `global_dirs` as `[mcp_arg_dir?, env_dir_0, env_dir_1, …]` so that the MCP
/// wire argument precedes ENV dirs and both precede the project justfile.
fn build_resolve_chain(root: &Path, global_dirs: &[PathBuf]) -> Vec<(ResolveLevel, PathBuf)> {
    let mut chain = Vec::new();

    // (1) Default: ~/.config/lds (always first, lowest priority)
    if let Some(home) = home_dir() {
        let default_global = home.join(".config/lds");
        if let Some(path) = find_justfile(&default_global) {
            chain.push((ResolveLevel::Global, path));
        }
    }

    // (2) Extra global dirs in declaration order (crux §2: Vec preserves insertion order)
    for dir in global_dirs {
        if let Some(path) = find_justfile(dir) {
            chain.push((ResolveLevel::Global, path));
        }
    }

    // (3) Project justfile (highest priority)
    if let Some(path) = find_justfile(root) {
        chain.push((ResolveLevel::Project, path));
    }

    chain
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

async fn dump_justfile(justfile: &Path) -> Result<Vec<JustRecipe>, RecipeError> {
    let output = tokio::process::Command::new("just")
        .arg("--justfile")
        .arg(justfile)
        .arg("--dump")
        .arg("--dump-format")
        .arg("json")
        .output()
        .await?; // io::Error -> RecipeError::Io via #[from]

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(RecipeError::JustDumpFailed {
            justfile: justfile.to_path_buf(),
            stderr,
        });
    }

    serde_json::from_slice::<JustDump>(&output.stdout)
        .map(|dump| dump.recipes.into_values().collect())
        .map_err(|source| RecipeError::JustDumpParse {
            justfile: justfile.to_path_buf(),
            source,
        })
}

fn is_allow_agent(recipe: &JustRecipe) -> bool {
    for attr in &recipe.attributes {
        match attr {
            JustAttribute::GroupObject { group } if group == ALLOW_AGENT_GROUP => return true,
            JustAttribute::Bare(s) if s == ALLOW_AGENT_GROUP => return true,
            _ => {}
        }
    }
    if let Some(ref doc) = recipe.doc {
        for token in doc.split_whitespace() {
            if token == LEGACY_ALLOW_AGENT_DOC {
                return true;
            }
        }
    }
    false
}

fn is_plugin(recipe: &JustRecipe) -> bool {
    for attr in &recipe.attributes {
        match attr {
            JustAttribute::GroupObject { group } if group == PLUGIN_GROUP => return true,
            JustAttribute::Bare(s) if s == PLUGIN_GROUP => return true,
            _ => {}
        }
    }
    false
}

#[derive(Debug, serde::Serialize)]
pub struct RecipeInfo {
    pub name: String,
    pub description: String,
    pub parameters: Vec<String>,
    pub source: ResolveInfo,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RecipeOutput {
    pub id: String,
    pub started_at: u64,
    pub duration_ms: u64,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub timed_out: bool,
    pub truncated: bool,
}

impl HasId for RecipeOutput {
    fn id(&self) -> &str {
        &self.id
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RecipeOutputSummary {
    pub id: String,
    pub started_at: u64,
    pub duration_ms: u64,
    pub exit_code: i32,
    pub timed_out: bool,
    pub truncated: bool,
}

impl From<&RecipeOutput> for RecipeOutputSummary {
    fn from(o: &RecipeOutput) -> Self {
        Self {
            id: o.id.clone(),
            started_at: o.started_at,
            duration_ms: o.duration_ms,
            exit_code: o.exit_code,
            timed_out: o.timed_out,
            truncated: o.truncated,
        }
    }
}

fn validate_content_key(key: &str) -> Result<(), RecipeError> {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return Err(RecipeError::InvalidContentKey(key.to_string())),
    }
    for c in chars {
        if !c.is_ascii_alphanumeric() && c != '_' {
            return Err(RecipeError::InvalidContentKey(key.to_string()));
        }
    }
    Ok(())
}

fn validate_arg_value(value: &str) -> Result<(), RecipeError> {
    if value.contains('\n') || value.contains('\r') {
        return Err(RecipeError::DangerousArgument);
    }
    Ok(())
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "system clock before unix epoch in now_epoch");
            std::time::Duration::default()
        })
        .as_secs()
}

fn exec_uuid() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "system clock before unix epoch in exec_uuid");
            std::time::Duration::default()
        })
        .as_nanos();
    let pid = std::process::id();
    format!("{ts:x}-{pid:x}")
}

fn find_justfile(dir: &Path) -> Option<PathBuf> {
    let candidates = ["justfile", "Justfile", ".justfile"];
    for name in &candidates {
        let path = dir.join(name);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

#[derive(Debug, Deserialize)]
struct JustDump {
    recipes: HashMap<String, JustRecipe>,
}

#[derive(Debug, Deserialize)]
struct JustRecipe {
    name: String,
    doc: Option<String>,
    #[serde(default)]
    attributes: Vec<JustAttribute>,
    #[serde(default)]
    parameters: Vec<JustParameter>,
    #[serde(default)]
    private: bool,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum JustAttribute {
    GroupObject {
        group: String,
    },
    Bare(String),
    #[allow(dead_code)]
    Other(serde_json::Value),
}

#[derive(Debug, Deserialize)]
struct JustParameter {
    name: String,
    #[serde(default)]
    default: Option<serde_json::Value>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PluginRecipe {
    pub name: String,
    pub description: String,
    pub parameters: Vec<PluginParam>,
    pub source: ResolveInfo,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PluginParam {
    pub name: String,
    pub default: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_key_valid_alpha() {
        assert!(validate_content_key("BODY").is_ok());
    }

    #[test]
    fn content_key_valid_alphanum_underscore() {
        assert!(validate_content_key("my_key_2").is_ok());
    }

    #[test]
    fn content_key_invalid_empty() {
        assert!(validate_content_key("").is_err());
    }

    #[test]
    fn content_key_invalid_digit_start() {
        assert!(validate_content_key("2key").is_err());
    }

    #[test]
    fn content_key_invalid_hyphen() {
        assert!(validate_content_key("my-key").is_err());
    }

    #[test]
    fn content_key_invalid_space() {
        assert!(validate_content_key("my key").is_err());
    }

    #[test]
    fn content_key_invalid_newline() {
        assert!(validate_content_key("key\n").is_err());
    }

    #[test]
    fn content_key_invalid_underscore_start() {
        assert!(validate_content_key("_key").is_err());
    }

    #[test]
    fn arg_value_valid_normal() {
        assert!(validate_arg_value("hello world").is_ok());
    }

    #[test]
    fn arg_value_valid_shell_metachar() {
        assert!(validate_arg_value("foo; rm -rf /").is_ok());
    }

    #[test]
    fn arg_value_invalid_newline() {
        assert!(validate_arg_value("line1\nline2").is_err());
    }

    #[test]
    fn arg_value_invalid_cr() {
        assert!(validate_arg_value("line1\rline2").is_err());
    }

    #[test]
    fn recipe_error_display() {
        let err = RecipeError::InvalidContentKey("bad-key".to_string());
        let msg = err.to_string();
        assert!(msg.contains("bad-key"));
        assert!(msg.contains("[A-Za-z]"));
    }

    #[test]
    fn recipe_error_just_dump_failed_display() {
        let err = RecipeError::JustDumpFailed {
            justfile: PathBuf::from("/tmp/justfile"),
            stderr: "syntax error".to_string(),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("/tmp/justfile"),
            "expected path in message: {msg}"
        );
        assert!(
            msg.contains("syntax error"),
            "expected stderr in message: {msg}"
        );
    }

    #[test]
    fn recipe_error_just_dump_parse_display() {
        // Construct a serde_json::Error by attempting to parse invalid JSON.
        let source = serde_json::from_str::<serde_json::Value>("not-json").unwrap_err();
        let err = RecipeError::JustDumpParse {
            justfile: PathBuf::from("/tmp/justfile"),
            source,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("/tmp/justfile"),
            "expected path in message: {msg}"
        );
    }

    #[test]
    fn ensure_alive_ok_when_root_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let session = Arc::new(
            lds_core::Session::new(lds_core::SessionConfig {
                root: tmp.path().to_path_buf(),
                timeout_secs: Some(10),
                ..Default::default()
            })
            .unwrap(),
        );
        assert!(session.ensure_alive().is_ok());
    }

    #[test]
    fn ensure_alive_err_when_root_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().to_path_buf();
        let session = Arc::new(
            lds_core::Session::new(lds_core::SessionConfig {
                root: path.clone(),
                timeout_secs: Some(10),
                ..Default::default()
            })
            .unwrap(),
        );
        std::fs::remove_dir_all(&path).unwrap();
        assert!(!path.exists());
        let err = session.ensure_alive().unwrap_err();
        assert!(
            err.to_string()
                .contains("session root path no longer exists, please call session_start again"),
            "expected K-239 substring, got: {err}"
        );
    }
}
