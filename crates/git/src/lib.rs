//! Git operations backed by [`git2`], with session-scoped write safety.
//!
//! Every public method returns a typed [`output`] struct wrapped in
//! [`anyhow::Result`]. The lds MCP layer serialises these structs with
//! `serde_json::to_string_pretty` so callers receive a stable JSON shape and
//! can access fields directly instead of parsing free-form text.
//!
//! Read operations (status, log, diff, worktree_list, remote inspection) are
//! always available. Write operations (commit, merge, worktree add/remove,
//! branch delete, reset) require the target path / branch to have been
//! created — or formally adopted via [`GitModule::session_release`] — by the
//! current session.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use lds_core::Session;

pub mod output;
mod read;
mod remote;
mod reset;
mod session;
mod write;

pub use output::{
    BranchDeleteOutput, BranchStatusOutput, CommitEntry, CommitOutput, DiffOutput, EntryStatus,
    FetchOutput, IsPushedOutput, LogOutput, MergeOutput, RemoteEntry, RemoteListOutput, ResetMode,
    ResetOutput, SessionReleaseOutput, StatusKind, StatusOutput, TagPushedOutput,
    UnpushedCommitsOutput, WorktreeAddOutput, WorktreeEntry, WorktreeListOutput,
    WorktreeRemoveOutput, WorktreeStateOutput,
};

/// Git module instance, tied to a [`Session`].
///
/// Tracks which worktrees and branches were created by this session.
/// Write operations check ownership before proceeding; read operations
/// bypass the check entirely.
#[derive(Debug)]
pub struct GitModule {
    session: Arc<Session>,
    /// Worktrees created by this session — only these can be committed to / removed.
    owned_worktrees: HashSet<PathBuf>,
    /// Branches created by this session — only these can be deleted / merged.
    owned_branches: HashSet<String>,
}

impl GitModule {
    pub fn new(session: Arc<Session>) -> Self {
        Self {
            session,
            owned_worktrees: HashSet::new(),
            owned_branches: HashSet::new(),
        }
    }

    pub fn register_worktree(&mut self, path: PathBuf) {
        self.owned_worktrees.insert(path);
    }

    pub fn is_owned(&self, path: &PathBuf) -> bool {
        self.owned_worktrees.contains(path)
    }

    pub fn ensure_owned(&self, path: &PathBuf) -> Result<()> {
        if !self.is_owned(path) {
            bail!(
                "worktree not owned by this session ({}): {}",
                self.session.id(),
                path.display()
            );
        }
        Ok(())
    }

    pub(crate) fn ensure_branch_owned(&self, branch: &str) -> Result<()> {
        if !self.owned_branches.contains(branch) {
            bail!(
                "branch not owned by this session ({}): {}",
                self.session.id(),
                branch,
            );
        }
        Ok(())
    }

    pub(crate) fn worktrees_dir(&self) -> PathBuf {
        self.session.root().join(".worktrees")
    }

    pub(crate) fn ensure_session_scope(&self, working_dir: &Path) -> Result<()> {
        if working_dir == self.session.root() {
            return Ok(());
        }
        let canon = working_dir
            .canonicalize()
            .unwrap_or_else(|_| working_dir.to_path_buf());
        if self.owned_worktrees.contains(&canon) {
            return Ok(());
        }
        if self.owned_worktrees.contains(working_dir) {
            return Ok(());
        }
        bail!(
            "working_dir not owned by this session ({}): {}",
            self.session.id(),
            working_dir.display(),
        );
    }

    pub(crate) fn session(&self) -> &Session {
        &self.session
    }

    pub(crate) fn register_branch(&mut self, branch: String) {
        self.owned_branches.insert(branch);
    }

    pub(crate) fn forget_worktree(&mut self, path: &Path) {
        let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.owned_worktrees.remove(&canon);
        self.owned_worktrees.remove(path);
    }
}

/// Run `git <args>` inside `cwd`, returning trimmed stdout on success.
///
/// Shared by every module that needs to shell out (fetch, ls-remote,
/// for-each-ref, worktree, commit, merge, reset). git2-rs is preferred for
/// pure read paths (statuses, revwalk, diff, graph_ahead_behind) because it
/// avoids spawning a subprocess and exposes typed data — but anything that
/// touches credentials, refspecs, or worktree-level porcelain is delegated
/// here to keep the implementation honest about what stock `git` would do.
pub(crate) fn git_cmd(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("failed to run git {}", args.first().unwrap_or(&"")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {}: {}", args.first().unwrap_or(&""), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Variant of [`git_cmd`] that merges stdout + stderr so callers can capture
/// transport diagnostics (typical for `git fetch`).
pub(crate) fn git_cmd_combined(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("failed to run git {}", args.first().unwrap_or(&"")))?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if !output.status.success() {
        let combined = if stderr.is_empty() { stdout } else { stderr };
        bail!("git {}: {}", args.first().unwrap_or(&""), combined);
    }

    Ok(match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout,
        (true, false) => stderr,
        (false, false) => format!("{stdout}\n{stderr}"),
    })
}
