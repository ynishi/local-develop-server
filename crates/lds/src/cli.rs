//! CLI subcommands for `lds`.
//!
//! Entry point: `run()` — called from `main()` when CLI arguments are present.
//! Provides `recipe-dir add|list|remove` to manage `~/.config/lds/config.toml`,
//! and `pack create|restore|inspect` to move a whole project between machines.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use lds_core::config::{Config, tilde_expand};
use lds_pack::{CreateOptions, Manifest, PackRules, RestoreOptions, RestoreReport, RuleOverrides};

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
    /// Bundle a whole project — `.git` and untracked local state — into one archive.
    Pack {
        #[command(subcommand)]
        action: PackAction,
    },
}

#[derive(Debug, Subcommand)]
pub enum PackAction {
    /// Pack a project directory into a single archive.
    Create {
        /// Project root to pack (default: current directory).
        #[arg(long)]
        root: Option<String>,
        /// Destination archive (default: `<project>-<timestamp>.pack` here).
        #[arg(long, short)]
        out: Option<String>,
        /// zstd compression level.
        #[arg(long, default_value_t = lds_pack::DEFAULT_COMPRESSION_LEVEL)]
        level: i32,
        /// Report what would be packed and skipped, without writing an archive.
        #[arg(long)]
        dry_run: bool,
    },
    /// Restore a pack into a directory.
    Restore {
        /// Archive to restore.
        archive: String,
        /// Destination directory (default: the project name, here).
        #[arg(long)]
        into: Option<String>,
        /// Unpack even if the destination exists (overwrites; never deletes
        /// files the pack does not carry).
        #[arg(long)]
        force: bool,
        /// Report what the restore would do here, without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Show what a pack contains without unpacking it.
    Inspect {
        /// Archive to inspect.
        archive: String,
        /// Also list every packed path (reads the whole archive).
        #[arg(long)]
        files: bool,
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
        Commands::Pack { action } => handle_pack(action),
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
    let expanded = tilde_expand(raw).with_context(|| format!("failed to expand path '{raw}'"))?;

    // 2. Make absolute (works even if the directory does not exist yet).
    let abs = std::path::absolute(&expanded)
        .with_context(|| format!("failed to make path absolute: {}", expanded.display()))?;

    // 3. Load current config (missing file → empty default).
    let config_path = default_config_path()?;
    let mut config = Config::load_or_default();

    // 4. Deduplicate — no-op with warning when already present.
    if config.recipes.dirs.contains(&abs) {
        eprintln!(
            "warn: '{}' is already in recipes.dirs — skipping",
            abs.display()
        );
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

// ---------------------------------------------------------------------------
// pack
// ---------------------------------------------------------------------------

/// Handle `pack create|restore|inspect`.
fn handle_pack(action: PackAction) -> Result<()> {
    match action {
        PackAction::Create {
            root,
            out,
            level,
            dry_run,
        } => cmd_pack_create(root, out, level, dry_run),
        PackAction::Restore {
            archive,
            into,
            force,
            dry_run,
        } => cmd_pack_restore(&archive, into, force, dry_run),
        PackAction::Inspect { archive, files } => cmd_pack_inspect(&archive, files),
    }
}

/// `pack create`: classify the project, write the archive, report what was left out.
fn cmd_pack_create(
    root: Option<String>,
    out: Option<String>,
    level: i32,
    dry_run: bool,
) -> Result<()> {
    let root = match root {
        Some(raw) => resolve_arg_path(&raw)?,
        None => std::env::current_dir().context("failed to read current directory")?,
    };

    let out = match out {
        Some(raw) => resolve_arg_path(&raw)?,
        None => default_pack_name(&root),
    };

    let rules = pack_rules_from_config()?;
    let customized = rules.is_customized();

    let mut opts = CreateOptions::new(&root, &out, env!("CARGO_PKG_VERSION"));
    opts.compression_level = level;
    opts.rules = rules;
    opts.dry_run = dry_run;

    let report = lds_pack::create(&opts).context("failed to create pack")?;
    let m = &report.manifest;

    if report.dry_run {
        println!(
            "dry run: {} -> {} (nothing written)",
            root.display(),
            out.display()
        );
        println!(
            "  would pack {} files, {} symlinks, {}",
            m.stats.file_count,
            m.stats.symlink_count,
            human_bytes(m.stats.total_bytes)
        );
    } else {
        println!(
            "packed: {} -> {}",
            root.display(),
            report.out_path.display()
        );
        println!(
            "  {} files, {} symlinks, {} -> {}",
            m.stats.file_count,
            m.stats.symlink_count,
            human_bytes(m.stats.total_bytes),
            human_bytes(report.compressed_bytes)
        );
    }

    if customized {
        println!("  (with [pack] overrides from config.toml)");
    }

    print_manifest_notes(m);
    Ok(())
}

/// Build classification rules from `[pack]` in `~/.config/lds/config.toml`.
///
/// A missing config leaves the built-in rules in force. A malformed glob is a
/// hard error rather than a warning: silently dropping it would leave the
/// operator believing a file is excluded when it is not.
fn pack_rules_from_config() -> Result<PackRules> {
    let cfg = Config::load_or_default();
    let overrides = RuleOverrides {
        secret_globs: cfg.pack.secret_globs.clone(),
        cache_dirs: cfg.pack.cache_dirs.clone(),
        keep: cfg.pack.keep.clone(),
    };
    PackRules::new(&overrides).context("invalid [pack] configuration in config.toml")
}

/// `pack restore`: unpack, repair worktree pointers, report what needs attention.
fn cmd_pack_restore(archive: &str, into: Option<String>, force: bool, dry_run: bool) -> Result<()> {
    let archive = resolve_arg_path(archive)?;

    let dest = match into {
        Some(raw) => resolve_arg_path(&raw)?,
        None => {
            let manifest = lds_pack::inspect(&archive)
                .with_context(|| format!("failed to read {}", archive.display()))?;
            std::env::current_dir()
                .context("failed to read current directory")?
                .join(manifest.project_name)
        }
    };

    let opts = RestoreOptions {
        archive: archive.clone(),
        dest,
        force,
        dry_run,
    };
    let report = lds_pack::restore(&opts).context("failed to restore pack")?;

    if report.dry_run {
        println!(
            "dry run: restore {} -> {} (nothing written)",
            archive.display(),
            report.dest.display()
        );
        println!("  would write {} entries", report.entries_written);
        if report.destination_exists {
            println!(
                "  destination exists: {} file(s) would be replaced, {} would remain untouched{}",
                report.would_overwrite.len(),
                report.would_remain.len(),
                if force { "" } else { " (needs --force)" }
            );
        }
    } else {
        println!(
            "restored: {} -> {} ({} entries)",
            archive.display(),
            report.dest.display(),
            report.entries_written
        );
    }
    if !report.rewritten_worktrees.is_empty() {
        println!(
            "  {} worktree pointers: {}",
            if report.dry_run {
                "would rewrite"
            } else {
                "rewrote"
            },
            report.rewritten_worktrees.join(", ")
        );
    }

    print_restore_attention(&report);
    Ok(())
}

/// `pack inspect`: read the manifest, optionally list every packed path.
fn cmd_pack_inspect(archive: &str, files: bool) -> Result<()> {
    let archive = resolve_arg_path(archive)?;
    let m = lds_pack::inspect(&archive)
        .with_context(|| format!("failed to read {}", archive.display()))?;

    println!("archive: {}", archive.display());
    println!("  project     {}", m.project_name);
    println!("  created     {}", m.created_at);
    println!("  source      {}", m.source_root);
    println!("  lds         {}", m.lds_version);
    println!("  format      {}", m.format_version);
    println!(
        "  payload     {} files, {} symlinks, {}",
        m.stats.file_count,
        m.stats.symlink_count,
        human_bytes(m.stats.total_bytes)
    );

    print_manifest_notes(&m);

    if files {
        println!();
        for path in lds_pack::list_payload_paths(&archive).context("failed to list payload")? {
            println!("{path}");
        }
    }

    Ok(())
}

/// Print the parts of a manifest an operator has to act on.
fn print_manifest_notes(m: &Manifest) {
    if !m.skipped_secret.is_empty() {
        println!("\nnot packed — secrets (move these yourself):");
        for s in &m.skipped_secret {
            println!("  {}  ({})", s.path, s.reason);
        }
    }

    if !m.skipped_cache.is_empty() {
        println!("\nnot packed — caches (regenerable):");
        for s in &m.skipped_cache {
            println!("  {}", s.path);
        }
    }

    if !m.symlinks.is_empty() {
        let outside = m.symlinks.iter().filter(|s| s.outside_root).count();
        println!(
            "\nsymlinks: {} recorded ({outside} pointing outside the project)",
            m.symlinks.len()
        );
        for s in m.symlinks.iter().filter(|s| s.outside_root) {
            println!("  {} -> {}", s.path, s.target);
        }
    }

    if m.claude.present {
        println!(
            "\n.claude/: packed as-is, {} symlinks",
            m.claude.symlink_count
        );
        for root in &m.claude.link_roots {
            println!("  links into {root}");
        }
    }

    if !m.worktrees.is_empty() {
        println!("\nworktrees:");
        for w in &m.worktrees {
            let state = if w.included {
                "packed"
            } else {
                "beside the project, packed separately"
            };
            println!("  {}  ({state})", w.name);
            if !w.included {
                println!("    was at {}", w.source_path);
            }
        }
    }

    if let Some(origin) = &m.worktree_of {
        println!("\nthis pack is a worktree named '{}'", origin.name);
        println!("  of the repository at {}", origin.parent_root);
        println!("  restore that beside this one and the pair wires itself up");
    }
}

/// Print the follow-up a restore leaves to the operator.
fn print_restore_attention(report: &RestoreReport) {
    if !report.needs_attention() {
        return;
    }

    println!(
        "\n{}:",
        if report.dry_run {
            "would need attention"
        } else {
            "needs attention"
        }
    );

    if report.dry_run && report.destination_exists {
        if !report.would_overwrite.is_empty() {
            println!(
                "  {} existing file(s) would be replaced:",
                report.would_overwrite.len()
            );
            for p in report.would_overwrite.iter().take(5) {
                println!("    {p}");
            }
            if report.would_overwrite.len() > 5 {
                println!("    … and {} more", report.would_overwrite.len() - 5);
            }
        }
        if !report.would_remain.is_empty() {
            println!(
                "  {} existing file(s) the pack does not carry would REMAIN (restore overwrites, it does not wipe):",
                report.would_remain.len()
            );
            for p in report.would_remain.iter().take(5) {
                println!("    {p}");
            }
            if report.would_remain.len() > 5 {
                println!("    … and {} more", report.would_remain.len() - 5);
            }
        }
    }

    if !report.dangling_symlinks.is_empty() {
        println!(
            "  {} symlink(s) {} dangle here:",
            report.dangling_symlinks.len(),
            if report.dry_run { "would" } else { "" }
        );
        for s in &report.dangling_symlinks {
            println!("    {} -> {}", s.path, s.target);
        }
    }

    if !report.missing_claude_link_roots.is_empty() {
        println!("  .claude/ links point at roots that are absent here:");
        for root in &report.missing_claude_link_roots {
            println!("    {root}");
        }
    }

    if !report.missing_worktrees.is_empty() {
        println!(
            "  worktrees registered outside the project root, not found here: {}",
            report.missing_worktrees.join(", ")
        );
        println!("    restore their own packs beside this one and the wiring completes");
    }

    if let Some(parent) = &report.missing_worktree_parent {
        println!("  this pack is a worktree; its repository is not beside it here");
        println!("    it was at {parent}; restore that pack and the wiring completes");
    }

    if !report.secrets_not_carried.is_empty() {
        println!("  secrets were not carried; move them yourself:");
        for s in &report.secrets_not_carried {
            println!("    {}", s.path);
        }
    }
}

/// Expand a tilde and make the path absolute.
fn resolve_arg_path(raw: &str) -> Result<PathBuf> {
    let expanded = tilde_expand(raw).with_context(|| format!("failed to expand path '{raw}'"))?;
    std::path::absolute(&expanded)
        .with_context(|| format!("failed to make path absolute: {}", expanded.display()))
}

/// Default archive name: `<project>-<timestamp>.pack` in the current directory.
fn default_pack_name(root: &Path) -> PathBuf {
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".to_string());
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    PathBuf::from(format!("{name}-{stamp}.pack"))
}

/// Format a byte count with a binary unit suffix.
fn human_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `recipe-dir remove <path>`: expand → absolute → retain all non-matching → write back.
fn cmd_remove(raw: &str) -> Result<()> {
    // 1. Expand and absolutize the target path the same way `add` does.
    let expanded = tilde_expand(raw).with_context(|| format!("failed to expand path '{raw}'"))?;
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
