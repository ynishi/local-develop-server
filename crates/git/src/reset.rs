//! Reset operations — destructive, so they're guarded by the same ownership
//! check as `commit` / `merge`.
//!
//! `git reset --hard` is the most reflog-heavy thing this crate does. We
//! capture HEAD before and after so callers can produce an audit line ("HEAD
//! moved from X to Y, mode=hard"), and so an undo path is at least
//! discoverable via the reflog rather than silently lost.

use std::path::Path;

use anyhow::{Result, bail};

use crate::output::{ResetMode, ResetOutput};
use crate::stash::dirty_paths;
use crate::{GitModule, TIMEOUT_LOCAL, git_cmd};

impl GitModule {
    /// Move HEAD to `target`, with `mode` controlling the working tree
    /// behaviour. The working directory MUST be owned by the current
    /// session — see [`GitModule::ensure_session_scope`].
    ///
    /// * [`ResetMode::Soft`]   — move HEAD only (`git reset --soft`)
    /// * [`ResetMode::Mixed`]  — also reset index but keep worktree (`--mixed`)
    /// * [`ResetMode::Hard`]   — also overwrite worktree (`--hard`)
    ///
    /// `force` only affects `Hard`: without it, a hard reset is refused when
    /// the repository has stash entries *and* the working tree is dirty —
    /// the signature of "a stash was applied and is being cleaned up with the
    /// biggest hammer available", which is precisely how stashed work gets
    /// destroyed. [`GitModule::stash_abort`] is the non-destructive undo;
    /// `force = true` is for callers who mean the reset regardless.
    pub async fn reset(
        &self,
        working_dir: &Path,
        mode: ResetMode,
        target: &str,
        force: bool,
    ) -> Result<ResetOutput> {
        self.ensure_session_scope(working_dir)?;

        if matches!(mode, ResetMode::Hard) && !force {
            self.ensure_no_stash_in_flight(working_dir).await?;
        }

        let previous_head = git_cmd(working_dir, &["rev-parse", "HEAD"], TIMEOUT_LOCAL).await?;
        let flag = match mode {
            ResetMode::Soft => "--soft",
            ResetMode::Mixed => "--mixed",
            ResetMode::Hard => "--hard",
        };
        git_cmd(working_dir, &["reset", flag, target], TIMEOUT_LOCAL).await?;
        let current_head = git_cmd(working_dir, &["rev-parse", "HEAD"], TIMEOUT_LOCAL).await?;

        Ok(ResetOutput {
            mode,
            target: target.to_string(),
            previous_head,
            current_head,
        })
    }

    /// Refuse when the repository carries stash entries and `working_dir` has
    /// uncommitted changes. Either condition alone is unremarkable; together
    /// they are indistinguishable from a half-verified `stash_apply`, and a
    /// `--hard` on top of that is unrecoverable.
    async fn ensure_no_stash_in_flight(&self, working_dir: &Path) -> Result<()> {
        let stashes = self.stash_list().await?.stashes;
        if stashes.is_empty() {
            return Ok(());
        }
        let (staged, unstaged) = dirty_paths(working_dir).await?;
        if staged.is_empty() && unstaged.is_empty() {
            return Ok(());
        }
        bail!(
            "reset --hard refused: {} stash entr(y|ies) exist and the working tree is dirty \
             (staged: [{}], unstaged: [{}]) — this looks like an applied stash. Use \
             git_stash_abort to undo the apply (the entry survives), or pass force=true if \
             the reset is intended.",
            stashes.len(),
            staged.join(", "),
            unstaged.join(", "),
        )
    }
}
