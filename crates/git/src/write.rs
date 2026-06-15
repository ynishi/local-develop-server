//! Mutating operations: commit, merge, branch delete, worktree add/remove.
//!
//! Every method here calls [`GitModule::ensure_session_scope`] or
//! [`GitModule::ensure_branch_owned`] (directly or via `worktree_remove` /
//! `branch_delete`) before touching state — that's what makes a multi-agent
//! setup safe.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::output::{
    BranchDeleteOutput, CommitOutput, MergeOutput, WorktreeAddOutput, WorktreeRemoveOutput,
};
use crate::{GitModule, git_cmd};

impl GitModule {
    /// Create a new worktree under `.worktrees/<name>` on a new branch.
    pub fn worktree_add(
        &mut self,
        name: &str,
        branch: &str,
        base_branch: Option<&str>,
    ) -> Result<WorktreeAddOutput> {
        let wt_dir = self.worktrees_dir();
        std::fs::create_dir_all(&wt_dir).context("failed to create .worktrees directory")?;

        let wt_path = wt_dir.join(name);
        if wt_path.exists() {
            bail!("worktree already exists: {}", wt_path.display());
        }

        let path_str = wt_path.to_str().unwrap_or(name);
        if let Some(base) = base_branch {
            git_cmd(
                self.session().root(),
                &["worktree", "add", "-b", branch, path_str, base],
            )?;
        } else {
            git_cmd(
                self.session().root(),
                &["worktree", "add", "-b", branch, path_str],
            )?;
        }

        let canon = wt_path.canonicalize().unwrap_or_else(|_| wt_path.clone());
        self.register_worktree(canon);
        self.register_branch(branch.to_string());

        Ok(WorktreeAddOutput {
            path: wt_path,
            branch: branch.to_string(),
            session: self.session().id().to_string(),
        })
    }

    /// Remove a session-owned worktree (force-removed, then forgotten).
    pub fn worktree_remove(&mut self, name: &str) -> Result<WorktreeRemoveOutput> {
        let wt_path = self.worktrees_dir().join(name);
        let canon = wt_path.canonicalize().unwrap_or_else(|_| wt_path.clone());

        self.ensure_owned(&canon)
            .or_else(|_| self.ensure_owned(&wt_path))?;

        git_cmd(
            self.session().root(),
            &[
                "worktree",
                "remove",
                "--force",
                wt_path.to_str().unwrap_or(name),
            ],
        )?;

        self.forget_worktree(&canon);
        self.forget_worktree(&wt_path);

        Ok(WorktreeRemoveOutput { path: wt_path })
    }

    /// Stage and commit changes in `working_dir`.
    ///
    /// When `paths` is `None` or empty, all changes (`git add -A`) are
    /// staged; otherwise only the listed paths are.
    pub fn commit(
        &self,
        working_dir: &Path,
        message: &str,
        paths: Option<&[String]>,
    ) -> Result<CommitOutput> {
        self.ensure_session_scope(working_dir)?;

        match paths {
            Some(ps) if !ps.is_empty() => {
                let mut args = vec!["add", "--"];
                let owned: Vec<&str> = ps.iter().map(|s| s.as_str()).collect();
                args.extend(owned);
                git_cmd(working_dir, &args)?;
            }
            _ => {
                git_cmd(working_dir, &["add", "-A"])?;
            }
        }

        git_cmd(working_dir, &["commit", "-m", message])?;

        let sha = git_cmd(working_dir, &["rev-parse", "HEAD"])?;
        let short_sha = sha[..7.min(sha.len())].to_string();
        let files_changed = git_cmd(working_dir, &["diff", "--name-only", "HEAD~1..HEAD"])
            .map(|out| out.lines().filter(|l| !l.is_empty()).count())
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "git diff HEAD~1..HEAD failed (initial commit?)");
                0
            });

        Ok(CommitOutput {
            sha,
            short_sha,
            message: message.to_string(),
            files_changed,
        })
    }

    /// Merge `branch` into `into_branch` via `--no-ff`. On failure, the
    /// merge is auto-aborted and the original error surfaces.
    pub fn merge(
        &self,
        branch: &str,
        into_branch: &str,
        working_dir: &Path,
    ) -> Result<MergeOutput> {
        self.ensure_session_scope(working_dir)?;

        let current = git_cmd(working_dir, &["branch", "--show-current"])?;
        if current != into_branch {
            git_cmd(working_dir, &["checkout", into_branch])?;
        }

        let merge_message = format!("Merge branch '{}' into {}", branch, into_branch);
        match git_cmd(
            working_dir,
            &["merge", "--no-ff", branch, "-m", &merge_message],
        ) {
            Ok(raw) => {
                let sha = git_cmd(working_dir, &["rev-parse", "HEAD"])?;
                let short_sha = sha[..7.min(sha.len())].to_string();
                Ok(MergeOutput {
                    branch: branch.to_string(),
                    into_branch: into_branch.to_string(),
                    sha,
                    short_sha,
                    raw,
                })
            }
            Err(e) => {
                let _ = git_cmd(working_dir, &["merge", "--abort"]);
                bail!("merge failed (aborted): {e}");
            }
        }
    }

    /// Delete a session-owned branch (refuses to delete unmerged work; use
    /// `git branch -D` directly if you really need that).
    pub fn branch_delete(&self, branch: &str) -> Result<BranchDeleteOutput> {
        self.ensure_branch_owned(branch)?;
        git_cmd(self.session().root(), &["branch", "-d", branch])?;
        Ok(BranchDeleteOutput {
            branch: branch.to_string(),
        })
    }
}

