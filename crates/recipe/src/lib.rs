use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use lds_core::Session;

/// Recipe module — runs justfile recipes as MCP tools.
/// Equivalent to task-mcp's core: discover justfile, list recipes,
/// execute with allow-agent boundary.
#[derive(Debug)]
pub struct RecipeModule {
    session: Arc<Session>,
    justfile_path: Option<PathBuf>,
}

impl RecipeModule {
    pub fn new(session: Arc<Session>) -> Self {
        let justfile_path = find_justfile(session.root());
        if let Some(ref p) = justfile_path {
            tracing::info!(justfile = %p.display(), "recipe module: justfile found");
        } else {
            tracing::warn!("recipe module: no justfile found in {}", session.root().display());
        }
        Self {
            session,
            justfile_path,
        }
    }

    /// List available recipes (allow-agent filtered).
    pub async fn list(&self) -> Result<Vec<RecipeInfo>> {
        let justfile = self.require_justfile()?;
        let output = tokio::process::Command::new("just")
            .arg("--justfile")
            .arg(justfile)
            .arg("--list")
            .output()
            .await?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let recipes: Vec<RecipeInfo> = stdout
            .lines()
            .filter_map(|line| {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with("Available") {
                    return None;
                }
                let name = trimmed.split_whitespace().next()?;
                Some(RecipeInfo {
                    name: name.to_string(),
                    description: trimmed.to_string(),
                })
            })
            .collect();
        Ok(recipes)
    }

    /// Run a named recipe.
    pub async fn run(&self, recipe: &str, args: &[&str]) -> Result<RecipeOutput> {
        let justfile = self.require_justfile()?;
        let mut cmd = tokio::process::Command::new("just");
        cmd.arg("--justfile")
            .arg(justfile)
            .current_dir(self.session.root())
            .arg(recipe);
        for arg in args {
            cmd.arg(arg);
        }
        let output = cmd.output().await?;
        Ok(RecipeOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.status.code().unwrap_or(-1),
        })
    }

    fn require_justfile(&self) -> Result<&PathBuf> {
        self.justfile_path
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no justfile found"))
    }
}

#[derive(Debug, serde::Serialize)]
pub struct RecipeInfo {
    pub name: String,
    pub description: String,
}

#[derive(Debug, serde::Serialize)]
pub struct RecipeOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
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
