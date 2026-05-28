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

use anyhow::{bail, Result};
use lds_core::log_store::{HasId, LogStore};
use lds_core::{truncate_output, Session};
use serde::Deserialize;

const ALLOW_AGENT_GROUP: &str = "allow-agent";
const LEGACY_ALLOW_AGENT_DOC: &str = "[allow-agent]";

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
    #[error("just error: {0}")]
    Just(String),
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
        let resolve_chain = build_resolve_chain(session.root(), session.global_recipe_dir());
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

    pub async fn list(&self) -> Result<Vec<RecipeInfo>> {
        self.merged_recipes().await
    }

    pub async fn run(
        &self,
        recipe: &str,
        args: &[&str],
        content: &HashMap<String, String>,
        timeout_override: Option<u64>,
    ) -> Result<RecipeOutput, RecipeError> {
        for arg in args {
            validate_arg_value(arg)?;
        }
        for key in content.keys() {
            validate_content_key(key)?;
        }

        let resolved = self
            .merged_recipes()
            .await
            .map_err(|e| RecipeError::Just(e.to_string()))?;
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

    async fn merged_recipes(&self) -> Result<Vec<RecipeInfo>> {
        let mut merged: HashMap<String, RecipeInfo> = HashMap::new();

        // Iterate from lowest priority (Global) to highest (Project).
        // Higher priority overwrites on name collision.
        for (level, justfile_path) in &self.resolve_chain {
            let recipes = dump_justfile(justfile_path).await?;
            for r in recipes {
                if r.private {
                    continue;
                }
                if self.mode == RecipeMode::AgentOnly && !is_allow_agent(&r) {
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

fn build_resolve_chain(root: &Path, global_dir: Option<&Path>) -> Vec<(ResolveLevel, PathBuf)> {
    let mut chain = Vec::new();

    // Global (lowest priority, inserted first → overwritten by later entries)
    if let Some(dir) = global_dir {
        if let Some(path) = find_justfile(dir) {
            chain.push((ResolveLevel::Global, path));
        }
    } else {
        // Default: ~/.config/lds/justfile
        if let Some(home) = home_dir() {
            let default_global = home.join(".config/lds");
            if let Some(path) = find_justfile(&default_global) {
                chain.push((ResolveLevel::Global, path));
            }
        }
    }

    // Project (highest priority)
    if let Some(path) = find_justfile(root) {
        chain.push((ResolveLevel::Project, path));
    }

    chain
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

async fn dump_justfile(justfile: &Path) -> Result<Vec<JustRecipe>> {
    let output = tokio::process::Command::new("just")
        .arg("--justfile")
        .arg(justfile)
        .arg("--dump")
        .arg("--dump-format")
        .arg("json")
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("just --dump failed for {}: {stderr}", justfile.display());
    }

    let dump: JustDump = serde_json::from_slice(&output.stdout)?;
    Ok(dump.recipes.into_values().collect())
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
        .unwrap_or_default()
        .as_secs()
}

fn exec_uuid() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
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
    GroupObject { group: String },
    Bare(String),
    #[allow(dead_code)]
    Other(serde_json::Value),
}

#[derive(Debug, Deserialize)]
struct JustParameter {
    name: String,
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
}
