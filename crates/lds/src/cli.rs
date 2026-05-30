//! CLI subcommands for `lds`.
//!
//! Entry point: `run()` — called from `main()` when CLI arguments are present.
//! Provides `recipe-dir add|list|remove` to manage `~/.config/lds/config.toml`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use lds_core::config::{Config, tilde_expand};

// ---------------------------------------------------------------------------
// CLI structure
// ---------------------------------------------------------------------------

/// lds — local develop server
#[derive(Debug, Parser)]
#[command(name = "lds", about = "Local develop server CLI")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Manage global recipe directories stored in `~/.config/lds/config.toml`.
    #[command(name = "recipe-dir")]
    RecipeDir {
        #[command(subcommand)]
        action: RecipeDirAction,
    },
}

#[derive(Debug, Subcommand)]
pub enum RecipeDirAction {
    /// Add a directory to the global recipe dirs list.
    Add {
        /// Path to the recipe directory (tilde is expanded automatically).
        path: String,
    },
    /// List all configured global recipe directories.
    List,
    /// Remove a directory from the global recipe dirs list.
    Remove {
        /// Path to remove (tilde is expanded; must match an entry exactly).
        path: String,
    },
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Execute the CLI subcommand parsed from `std::env::args`.
///
/// # Errors
///
/// Returns an `anyhow::Error` on any I/O or config error; `main()` prints the
/// message to stderr and exits with code 1.
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::RecipeDir { action } => handle_recipe_dir(action),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Determine the default config.toml path (`~/.config/lds/config.toml`).
fn default_config_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("HOME directory is not set")?;
    Ok(home.join(".config/lds/config.toml"))
}

/// Handle `recipe-dir add|list|remove`.
fn handle_recipe_dir(action: RecipeDirAction) -> Result<()> {
    match action {
        RecipeDirAction::Add { path } => cmd_add(&path),
        RecipeDirAction::List => cmd_list(),
        RecipeDirAction::Remove { path } => cmd_remove(&path),
    }
}

/// `recipe-dir add <path>`: tilde-expand → absolute → deduplicate → patch-safe write.
fn cmd_add(raw: &str) -> Result<()> {
    // 1. Expand tilde (crux 2: tilde literal must not reach disk).
    let expanded =
        tilde_expand(raw).with_context(|| format!("failed to expand path '{raw}'"))?;

    // 2. Make absolute (works even if the directory does not exist yet).
    let abs = std::path::absolute(&expanded)
        .with_context(|| format!("failed to make path absolute: {}", expanded.display()))?;

    // 3. Load current config (missing file → empty default).
    let config_path = default_config_path()?;
    let mut config = Config::load_or_default();

    // 4. Deduplicate — no-op with warning when already present.
    if config.recipes.dirs.contains(&abs) {
        eprintln!("warn: '{}' is already in recipes.dirs — skipping", abs.display());
        return Ok(());
    }

    // 5. Append and write back (patch-safe via toml_edit).
    config.recipes.dirs.push(abs.clone());
    Config::save(&config_path, &config.recipes.dirs)
        .with_context(|| format!("failed to save config at {}", config_path.display()))?;

    println!("added: {}", abs.display());
    Ok(())
}

/// `recipe-dir list`: print one path per line in declaration order.
fn cmd_list() -> Result<()> {
    let config = Config::load_or_default();
    for dir in &config.recipes.dirs {
        println!("{}", dir.display());
    }
    Ok(())
}

/// `recipe-dir remove <path>`: expand → absolute → retain all non-matching → write back.
fn cmd_remove(raw: &str) -> Result<()> {
    // 1. Expand and absolutize the target path the same way `add` does.
    let expanded =
        tilde_expand(raw).with_context(|| format!("failed to expand path '{raw}'"))?;
    let target = std::path::absolute(&expanded)
        .with_context(|| format!("failed to make path absolute: {}", expanded.display()))?;

    // 2. Load current config.
    let config_path = default_config_path()?;
    let mut config = Config::load_or_default();

    // 3. Remove matching entries and detect if anything changed.
    let before = config.recipes.dirs.len();
    config.recipes.dirs.retain(|p| p != &target);

    if config.recipes.dirs.len() == before {
        eprintln!("error: '{}' not found in recipes.dirs", target.display());
        std::process::exit(1);
    }

    // 4. Write back (patch-safe).
    Config::save(&config_path, &config.recipes.dirs)
        .with_context(|| format!("failed to save config at {}", config_path.display()))?;

    println!("removed: {}", target.display());
    Ok(())
}
