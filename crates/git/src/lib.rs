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
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use lds_core::Session;

pub mod output;
mod read;
mod remote;
mod reset;
mod session;
mod write;

pub use read::LogFilters;

pub use output::{
    BranchDeleteOutput, BranchStatusOutput, CommitEntry, CommitOutput, DiffOutput, EntryStatus,
    FetchOutput, IsPushedOutput, LogOutput, MergeOutput, OtherStagedMode, RemoteEntry,
    RemoteListOutput, ResetMode, ResetOutput, SessionReleaseOutput, StatusKind, StatusOutput,
    TagPushedOutput, UnpushedCommitsOutput, WorktreeAddOutput, WorktreeEntry, WorktreeListOutput,
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

    /// Session-scoped worktree root — the directory where
    /// [`GitModule::worktree_add`] places worktrees on newly-created
    /// branches. Sourced from [`Session::worktrees_dir`], which resolves
    /// [`SessionConfig::worktrees_dir`] (explicit) → env
    /// `LDS_WORKTREES_DIR` → `<session_root>/.worktrees` (default). See
    /// [`SessionConfig::worktrees_dir`] for the setup expectation
    /// (parent-repo gitignore requirement).
    pub(crate) fn worktrees_dir(&self) -> PathBuf {
        self.session.worktrees_dir().to_path_buf()
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
pub(crate) async fn git_cmd(cwd: &Path, args: &[&str], timeout: Duration) -> Result<String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args).current_dir(cwd).kill_on_drop(true);

    let result = tokio::time::timeout(timeout, cmd.output()).await;
    let output = match result {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return Err(anyhow::Error::from(e))
                .with_context(|| format!("failed to run git {}", args.first().unwrap_or(&"")));
        }
        Err(_elapsed) => {
            bail!(
                "git {}: timed out after {}s",
                args.first().unwrap_or(&""),
                timeout.as_secs()
            );
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {}: {}", args.first().unwrap_or(&""), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Variant of [`git_cmd`] that merges stdout + stderr so callers can capture
/// transport diagnostics (typical for `git fetch`).
pub(crate) async fn git_cmd_combined(
    cwd: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args).current_dir(cwd).kill_on_drop(true);

    let result = tokio::time::timeout(timeout, cmd.output()).await;
    let output = match result {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return Err(anyhow::Error::from(e))
                .with_context(|| format!("failed to run git {}", args.first().unwrap_or(&"")));
        }
        Err(_elapsed) => {
            bail!(
                "git {}: timed out after {}s",
                args.first().unwrap_or(&""),
                timeout.as_secs()
            );
        }
    };

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

/// Timeout for git subcommands that only touch local refs / on-disk state
/// (rev-parse HEAD, status, log, diff, branch --show-current, worktree list,
/// commit, merge, worktree add/remove, branch -d, reset, check-ignore,
/// rev-parse @{upstream}, for-each-ref, rev-list --count).
pub(crate) const TIMEOUT_LOCAL: Duration = Duration::from_secs(10);

/// Timeout for git subcommands that hit the network (fetch, ls-remote,
/// anything that would talk to a remote transport).
pub(crate) const TIMEOUT_NETWORK: Duration = Duration::from_secs(60);

#[cfg(test)]
mod tests {
    use super::*;

    /// A pathologically short timeout should return an anyhow Error whose
    /// message contains the `timed out after {}s` literal. This is the
    /// grep-cross-check for the message contract (Acceptance 8) — the same
    /// path fetch / ls-remote take on network hang, exercised without an
    /// actually hanging endpoint.
    #[tokio::test]
    async fn git_cmd_reports_timeout_with_literal_message() {
        let tmp = std::env::temp_dir();
        let err = git_cmd(&tmp, &["version"], Duration::from_nanos(1))
            .await
            .expect_err("nanosecond timeout must trip");
        let msg = err.to_string();
        assert!(
            msg.contains("timed out after"),
            "expected 'timed out after' literal, got: {msg}"
        );
        assert!(
            msg.contains("git version"),
            "expected subcommand name in message, got: {msg}"
        );
    }

    #[tokio::test]
    async fn git_cmd_combined_reports_timeout_with_literal_message() {
        let tmp = std::env::temp_dir();
        let err = git_cmd_combined(&tmp, &["version"], Duration::from_nanos(1))
            .await
            .expect_err("nanosecond timeout must trip");
        assert!(
            err.to_string().contains("timed out after"),
            "expected 'timed out after' literal, got: {err}"
        );
    }
}
