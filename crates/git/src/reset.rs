//! Reset operations — destructive, so they're guarded by the same ownership
//! check as `commit` / `merge`.
//!
//! `git reset --hard` is the most reflog-heavy thing this crate does. We
//! capture HEAD before and after so callers can produce an audit line ("HEAD
//! moved from X to Y, mode=hard"), and so an undo path is at least
//! discoverable via the reflog rather than silently lost.

use std::path::Path;

use anyhow::Result;

use crate::output::{ResetMode, ResetOutput};
use crate::{GitModule, git_cmd};

impl GitModule {
    /// Move HEAD to `target`, with `mode` controlling the working tree
    /// behaviour. The working directory MUST be owned by the current
    /// session — see [`GitModule::ensure_session_scope`].
    ///
    /// * [`ResetMode::Soft`]   — move HEAD only (`git reset --soft`)
    /// * [`ResetMode::Mixed`]  — also reset index but keep worktree (`--mixed`)
    /// * [`ResetMode::Hard`]   — also overwrite worktree (`--hard`)
    pub fn reset(&self, working_dir: &Path, mode: ResetMode, target: &str) -> Result<ResetOutput> {
        self.ensure_session_scope(working_dir)?;

        let previous_head = git_cmd(working_dir, &["rev-parse", "HEAD"])?;
        let flag = match mode {
            ResetMode::Soft => "--soft",
            ResetMode::Mixed => "--mixed",
            ResetMode::Hard => "--hard",
        };
        git_cmd(working_dir, &["reset", flag, target])?;
        let current_head = git_cmd(working_dir, &["rev-parse", "HEAD"])?;

        Ok(ResetOutput {
            mode,
            target: target.to_string(),
            previous_head,
            current_head,
        })
    }
}
