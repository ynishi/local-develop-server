use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Result};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecipeMode {
    AgentOnly,
    Unrestricted,
}

impl Default for RecipeMode {
    fn default() -> Self {
        Self::AgentOnly
    }
}

#[derive(Debug)]
pub struct RecipeModule {
    session: Arc<Session>,
    resolve_chain: Vec<(ResolveLevel, PathBuf)>,
    mode: RecipeMode,
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
        }
    }

    pub async fn list(&self) -> Result<Vec<RecipeInfo>> {
        self.merged_recipes().await
    }

    pub async fn run(&self, recipe: &str, args: &[&str], content: &HashMap<String, String>) -> Result<RecipeOutput> {
        let resolved = self.merged_recipes().await?;
        let recipe_info = resolved.iter().find(|r| r.name == recipe);

        if self.mode == RecipeMode::AgentOnly {
            if recipe_info.is_none() {
                bail!(
                    "recipe '{}' is not in the allow-agent group — not available in agent-only mode",
                    recipe
                );
            }
        }

        let source_path = recipe_info
            .map(|r| r.source.source_path.clone())
            .ok_or_else(|| anyhow::anyhow!("recipe '{}' not found", recipe))?;

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

        let timeout = self.session.timeout();
        let result = tokio::time::timeout(timeout, cmd.output()).await;

        match result {
            Ok(Ok(output)) => {
                let max = self.session.max_output();
                let (stdout, stdout_trunc) = truncate_output(&output.stdout, max);
                let (stderr, stderr_trunc) = truncate_output(&output.stderr, max);
                Ok(RecipeOutput {
                    stdout,
                    stderr,
                    exit_code: output.status.code().unwrap_or(-1),
                    timed_out: false,
                    truncated: stdout_trunc || stderr_trunc,
                })
            }
            Ok(Err(e)) => Err(e.into()),
            Err(_) => Ok(RecipeOutput {
                stdout: String::new(),
                stderr: format!("recipe '{}' timed out after {}s", recipe, timeout.as_secs()),
                exit_code: -1,
                timed_out: true,
                truncated: false,
            }),
        }
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

#[derive(Debug, serde::Serialize)]
pub struct RecipeOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub timed_out: bool,
    pub truncated: bool,
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
