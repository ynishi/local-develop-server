use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Result};
use lds_core::{truncate_output, Session};
use serde::Deserialize;

const ALLOW_AGENT_GROUP: &str = "allow-agent";
const LEGACY_ALLOW_AGENT_DOC: &str = "[allow-agent]";

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
    justfile_path: Option<PathBuf>,
    mode: RecipeMode,
}

impl RecipeModule {
    pub fn new(session: Arc<Session>) -> Self {
        Self::with_mode(session, RecipeMode::default())
    }

    pub fn with_mode(session: Arc<Session>, mode: RecipeMode) -> Self {
        let justfile_path = find_justfile(session.root());
        if let Some(ref p) = justfile_path {
            tracing::info!(justfile = %p.display(), ?mode, "recipe module: justfile found");
        } else {
            tracing::warn!("recipe module: no justfile found in {}", session.root().display());
        }
        Self {
            session,
            justfile_path,
            mode,
        }
    }

    pub async fn list(&self) -> Result<Vec<RecipeInfo>> {
        let recipes = self.dump_recipes().await?;
        Ok(recipes)
    }

    pub async fn run(&self, recipe: &str, args: &[&str]) -> Result<RecipeOutput> {
        let justfile = self.require_justfile()?;

        if self.mode == RecipeMode::AgentOnly {
            let allowed = self.dump_recipes().await?;
            if !allowed.iter().any(|r| r.name == recipe) {
                bail!(
                    "recipe '{}' is not in the allow-agent group — not available in agent-only mode",
                    recipe
                );
            }
        }

        let mut cmd = tokio::process::Command::new("just");
        cmd.arg("--justfile")
            .arg(justfile)
            .current_dir(self.session.root())
            .arg(recipe);
        for arg in args {
            cmd.arg(arg);
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

    async fn dump_recipes(&self) -> Result<Vec<RecipeInfo>> {
        let justfile = self.require_justfile()?;
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
            bail!("just --dump failed: {stderr}");
        }

        let dump: JustDump = serde_json::from_slice(&output.stdout)?;
        let mut recipes: Vec<RecipeInfo> = dump
            .recipes
            .into_values()
            .filter(|r| !r.private)
            .filter(|r| self.mode == RecipeMode::Unrestricted || is_allow_agent(r))
            .map(|r| RecipeInfo {
                name: r.name,
                description: r.doc.unwrap_or_default(),
                parameters: r
                    .parameters
                    .into_iter()
                    .map(|p| p.name)
                    .collect(),
            })
            .collect();
        recipes.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(recipes)
    }

    fn require_justfile(&self) -> Result<&PathBuf> {
        self.justfile_path
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no justfile found"))
    }
}

fn is_allow_agent(recipe: &JustRecipe) -> bool {
    for attr in &recipe.attributes {
        if let JustAttribute::Group { group } = attr {
            if group == ALLOW_AGENT_GROUP {
                return true;
            }
        }
    }
    if let Some(ref doc) = recipe.doc {
        if doc.contains(LEGACY_ALLOW_AGENT_DOC) {
            return true;
        }
    }
    false
}

#[derive(Debug, serde::Serialize)]
pub struct RecipeInfo {
    pub name: String,
    pub description: String,
    pub parameters: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct RecipeOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub timed_out: bool,
    pub truncated: bool,
}

fn find_justfile(root: &std::path::Path) -> Option<PathBuf> {
    let candidates = ["justfile", "Justfile", ".justfile"];
    for name in &candidates {
        let path = root.join(name);
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
    Group { group: String },
    #[allow(dead_code)]
    Other(serde_json::Value),
}

#[derive(Debug, Deserialize)]
struct JustParameter {
    name: String,
}
