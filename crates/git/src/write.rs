//! Mutating operations: commit, merge, branch delete, worktree add/remove.
//!
//! Every method here calls [`GitModule::ensure_session_scope`] or
//! [`GitModule::ensure_branch_owned`] (directly or via `worktree_remove` /
//! `branch_delete`) before touching state — that's what makes a multi-agent
//! setup safe.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::output::{
    BranchDeleteOutput, CommitOutput, MergeOutput, OtherStagedMode, WorktreeAddOutput,
    WorktreeRemoveOutput,
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
    /// - `only == None` (or an empty slice): sweeps every change with
    ///   `git add -A` and commits. `other_staged` is ignored. Kept as the
    ///   backward-compatible default so pre-existing callers see identical
    ///   behaviour.
    /// - `only == Some(paths)`: commits exactly `paths`. When the index
    ///   already carries other staged work, the call is *transactional*
    ///   under `other_staged`:
    ///   * `Stop` — return an error, leave the index untouched. Safe
    ///     default, forces the caller to reconcile explicitly.
    ///   * `Restage` — unstage the intruding paths, stage + commit `only`,
    ///     then re-stage the intruders. The resulting commit contains
    ///     exactly `only`; the pre-existing staged work stays in the index.
    ///
    /// Untracked files listed in `only` are staged and committed like any
    /// other path; anything not in `only` is never touched.
    pub fn commit(
        &self,
        working_dir: &Path,
        message: &str,
        only: Option<&[String]>,
        other_staged: OtherStagedMode,
    ) -> Result<CommitOutput> {
        self.ensure_session_scope(working_dir)?;

        match only {
            None | Some([]) => {
                git_cmd(working_dir, &["add", "-A"])?;
            }
            Some(ps) => {
                let intruders = detect_other_staged(working_dir, ps)?;
                if !intruders.is_empty() {
                    match other_staged {
                        OtherStagedMode::Stop => {
                            bail!(
                                "commit aborted: index carries staged paths outside \
                                 the requested set (other_staged=stop): {}",
                                intruders.join(", ")
                            );
                        }
                        OtherStagedMode::Restage => {
                            unstage(working_dir, &intruders)?;
                            stage(working_dir, ps)?;
                            git_cmd(working_dir, &["commit", "-m", message])?;
                            let output = commit_output(working_dir, message);
                            // Best-effort re-stage; propagate a stage failure
                            // so the caller sees the intruders are now sitting
                            // in the worktree instead of the index.
                            stage(working_dir, &intruders)?;
                            return output;
                        }
                    }
                }
                stage(working_dir, ps)?;
            }
        }

        git_cmd(working_dir, &["commit", "-m", message])?;
        commit_output(working_dir, message)
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

/// Return staged paths (via `git diff --cached --name-only`) that are not
/// in `only`. Empty result means the index matches the commit intent.
fn detect_other_staged(working_dir: &Path, only: &[String]) -> Result<Vec<String>> {
    let staged_raw = git_cmd(working_dir, &["diff", "--cached", "--name-only"])?;
    let only_set: std::collections::HashSet<&str> = only.iter().map(|s| s.as_str()).collect();
    Ok(staged_raw
        .lines()
        .filter(|l| !l.is_empty())
        .filter(|l| !only_set.contains(*l))
        .map(|s| s.to_string())
        .collect())
}

/// `git add -- <paths>`. Ignored when `paths` is empty (no-op is not an error).
fn stage(working_dir: &Path, paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut args = vec!["add", "--"];
    args.extend(paths.iter().map(|s| s.as_str()));
    git_cmd(working_dir, &args)?;
    Ok(())
}

/// Unstage `paths` while keeping worktree changes intact. Uses `git reset --`
/// (index-only) so both modified-tracked and newly-staged files fall out of
/// the index without touching what's on disk.
fn unstage(working_dir: &Path, paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut args = vec!["reset", "--"];
    args.extend(paths.iter().map(|s| s.as_str()));
    git_cmd(working_dir, &args)?;
    Ok(())
}

/// Build the [`CommitOutput`] for the commit that HEAD currently points at.
fn commit_output(working_dir: &Path, message: &str) -> Result<CommitOutput> {
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
