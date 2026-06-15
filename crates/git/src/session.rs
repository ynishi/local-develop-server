//! Session helpers: adoption of orphan worktrees / branches that a previous
//! session owned but never released.
//!
//! `session_release` is the counterpart to `worktree_add` — when an MCP
//! client crashes and leaves `.worktrees/foo` on disk, the next session can
//! adopt it (and the branch checked out inside it) so that the normal
//! `worktree_remove` / `branch_delete` ownership checks pass.

use std::path::PathBuf;

use anyhow::Result;

use crate::output::SessionReleaseOutput;
use crate::{GitModule, git_cmd};

impl GitModule {
    /// Adopt orphan worktrees under `.worktrees/` (those known to `git
    /// worktree list` but not yet owned by this session). Any branches
    /// currently checked out inside them are adopted at the same time so
    /// the normal `branch_delete` ownership check will pass.
    ///
    /// Returns the set of worktrees / branches that were newly adopted.
    /// Already-owned entries are skipped silently — calling this repeatedly
    /// is idempotent.
    pub fn session_release(&mut self) -> Result<SessionReleaseOutput> {
        let raw = git_cmd(
            self.session().root(),
            &["worktree", "list", "--porcelain"],
        )?;

        let mut adopted_worktrees: Vec<PathBuf> = Vec::new();
        let mut adopted_branches: Vec<String> = Vec::new();

        let worktrees_dir = self.worktrees_dir();
        let root = self.session().root().to_path_buf();

        // Canonicalise once up-front so symlink-resolved tempdirs (e.g. macOS
        // `/var/...` vs `/private/var/...`) don't make `starts_with` lie.
        let canonical_worktrees_dir = worktrees_dir
            .canonicalize()
            .unwrap_or_else(|_| worktrees_dir.clone());
        let canonical_root = root.canonicalize().unwrap_or_else(|_| root.clone());

        let mut current_path: Option<PathBuf> = None;
        let mut current_branch: Option<String> = None;

        let entries: Vec<(PathBuf, Option<String>)> = {
            let mut acc = Vec::new();
            for line in raw.lines() {
                if line.is_empty() {
                    if let Some(p) = current_path.take() {
                        acc.push((p, current_branch.take()));
                    }
                    continue;
                }
                if let Some(p) = line.strip_prefix("worktree ") {
                    current_path = Some(PathBuf::from(p));
                } else if let Some(b) = line.strip_prefix("branch ") {
                    current_branch = Some(b.trim_start_matches("refs/heads/").to_string());
                }
            }
            if let Some(p) = current_path {
                acc.push((p, current_branch));
            }
            acc
        };

        for (path, branch) in entries {
            let canonical_path = path.canonicalize().unwrap_or_else(|_| path.clone());

            // Never adopt the main worktree — that's the session root, which
            // is implicitly addressable but not ownership-tracked.
            if canonical_path == canonical_root {
                continue;
            }
            // Only adopt worktrees under `.worktrees/` — anything outside is
            // probably user-managed and shouldn't be silently claimed.
            if !canonical_path.starts_with(&canonical_worktrees_dir) {
                continue;
            }
            if self.is_owned(&canonical_path) || self.is_owned(&path) {
                continue;
            }
            self.register_worktree(canonical_path.clone());
            adopted_worktrees.push(path.clone());
            if let Some(b) = branch
                && !self.owned_branches.contains(&b)
            {
                self.register_branch(b.clone());
                adopted_branches.push(b);
            }
        }

        Ok(SessionReleaseOutput {
            adopted_worktrees,
            adopted_branches,
        })
    }
}
