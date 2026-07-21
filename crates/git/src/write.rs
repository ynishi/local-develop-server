//! Mutating operations: commit, merge, branch delete, worktree add/remove.
//!
//! Every method here calls [`GitModule::ensure_session_scope`] or
//! [`GitModule::ensure_branch_owned`] (directly or via `worktree_remove` /
//! `branch_delete`) before touching state — that's what makes a multi-agent
//! setup safe.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::output::{
    BranchDeleteOutput, CommitOutput, DotfileWarning, MergeOutput, OtherStagedMode,
    WorktreeAddOutput, WorktreeRemoveOutput,
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
    ///
    /// **Dotfile / dot-dir safeguard.** Any candidate whose path contains a
    /// `.`-prefixed component (`.env`, `.claude/CLAUDE.md`,
    /// `.github/workflows/ci.yml`, `foo/.hidden`) is classified before
    /// staging:
    ///
    /// * *tracked* → committed as usual, but recorded in
    ///   [`CommitOutput::dotfile_warnings`] so pre-publish review can catch
    ///   unintended edits to `.gitignore` / workflow files / etc.
    /// * *untracked, not in `.gitignore`* → dropped from staging + recorded
    ///   in both `dotfile_warnings` and [`CommitOutput::dotfile_skipped`].
    /// * *untracked, in `.gitignore`* → silently skipped (matches git's
    ///   default; `git status --porcelain` doesn't surface them either).
    /// * `force_dot=true` → suppresses the entire mechanism: every candidate
    ///   is staged verbatim and no warnings are emitted.
    pub fn commit(
        &self,
        working_dir: &Path,
        message: &str,
        only: Option<&[String]>,
        other_staged: OtherStagedMode,
        force_dot: bool,
    ) -> Result<CommitOutput> {
        self.ensure_session_scope(working_dir)?;

        let (warnings, skipped) = match only {
            None | Some([]) => {
                let candidates = enumerate_changes(working_dir)?;
                if force_dot || !candidates.iter().any(|p| is_dotfile_path(p)) {
                    // Fast path: preserves the original `git add -A` sweep
                    // whenever no dotfile is involved (or the caller opted
                    // out via force_dot).
                    git_cmd(working_dir, &["add", "-A"])?;
                    (Vec::new(), Vec::new())
                } else {
                    let cls = classify_paths(working_dir, &candidates, false)?;
                    stage_add_all(working_dir, &cls.stage)?;
                    (cls.warnings, cls.skipped)
                }
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
                            let cls = classify_paths(working_dir, ps, force_dot)?;
                            stage(working_dir, &cls.stage)?;
                            git_cmd(working_dir, &["commit", "-m", message])?;
                            let output =
                                commit_output(working_dir, message, cls.warnings, cls.skipped);
                            // Best-effort re-stage; propagate a stage failure
                            // so the caller sees the intruders are now sitting
                            // in the worktree instead of the index.
                            stage(working_dir, &intruders)?;
                            return output;
                        }
                    }
                }
                let cls = classify_paths(working_dir, ps, force_dot)?;
                stage(working_dir, &cls.stage)?;
                (cls.warnings, cls.skipped)
            }
        };

        git_cmd(working_dir, &["commit", "-m", message])?;
        commit_output(working_dir, message, warnings, skipped)
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
fn commit_output(
    working_dir: &Path,
    message: &str,
    dotfile_warnings: Vec<DotfileWarning>,
    dotfile_skipped: Vec<String>,
) -> Result<CommitOutput> {
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
        dotfile_warnings,
        dotfile_skipped,
    })
}

/// Result of classifying a candidate path set against the dotfile safeguard.
struct Classification {
    /// Paths that should be staged and included in the commit. Contains every
    /// non-dotfile candidate plus tracked dotfiles (surface change) plus
    /// everything when `force_dot=true`.
    stage: Vec<String>,
    /// Dotfile paths observed during the walk (tracked + untracked-not-ignored).
    /// Empty when `force_dot=true`.
    warnings: Vec<DotfileWarning>,
    /// Dotfile paths dropped from staging (untracked + not in `.gitignore`).
    /// Silent-ignored dotfiles are not tracked here (they're already invisible
    /// to `git add -A` and to `git status --porcelain`).
    skipped: Vec<String>,
}

/// Split `candidates` into stage / warn / skip buckets per the dotfile safeguard.
fn classify_paths(
    working_dir: &Path,
    candidates: &[String],
    force_dot: bool,
) -> Result<Classification> {
    let mut stage = Vec::new();
    let mut warnings = Vec::new();
    let mut skipped = Vec::new();

    for p in candidates {
        if force_dot || !is_dotfile_path(p) {
            stage.push(p.clone());
            continue;
        }
        // Dotfile: classify tracked vs untracked.
        let tracked = is_tracked(working_dir, p)?;
        if tracked {
            stage.push(p.clone());
            warnings.push(DotfileWarning {
                path: p.clone(),
                tracked: true,
                in_gitignore: false,
            });
        } else if is_ignored(working_dir, p) {
            // Silent skip — matches git's default behaviour.
            skipped.push(p.clone());
        } else {
            skipped.push(p.clone());
            warnings.push(DotfileWarning {
                path: p.clone(),
                tracked: false,
                in_gitignore: false,
            });
        }
    }

    Ok(Classification {
        stage,
        warnings,
        skipped,
    })
}

/// `true` when any `/`-separated component of `p` starts with `.` (excluding
/// `.` / `..` which aren't dotfiles). Matches `.env` at root, nested
/// `foo/.env`, `.claude/CLAUDE.md`, `.github/workflows/ci.yml`.
fn is_dotfile_path(p: &str) -> bool {
    p.split('/')
        .any(|c| c.starts_with('.') && c != "." && c != "..")
}

/// Enumerate the paths that `git add -A` would sweep — worktree modifications
/// plus untracked-and-not-ignored files. Ignored files never surface here (nor
/// in `git add -A`).
///
/// Uses `--porcelain=v1 -z --untracked-files=all` so the output is
/// nul-delimited, column-stable, AND expands untracked directories into their
/// contained paths. Without `-uall`, git collapses an untracked directory to a
/// single `?? dir/` entry — a nested dotfile like `workspace/.journal.db`
/// then never reaches [`classify_paths`], and the safeguard silently misses
/// it. `git_cmd`'s trimmed stdout would also strip the leading space from a
/// single-line ` M path` status code and break the XY-column offset.
fn enumerate_changes(working_dir: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .current_dir(working_dir)
        .output()
        .context("failed to run git status --porcelain")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git status --porcelain: {}", stderr.trim());
    }

    let raw = String::from_utf8_lossy(&output.stdout);
    let mut paths = Vec::new();
    // Records are nul-terminated. Rename records emit two records back-to-back:
    // "R  newpath\0oldpath\0" — we only care about the new path, so the oldpath
    // record is consumed via the peek-and-skip below.
    let mut it = raw.split('\0').peekable();
    while let Some(rec) = it.next() {
        if rec.len() < 4 {
            continue;
        }
        let status = &rec[..2];
        let path = &rec[3..];
        if status.starts_with('R') || status.starts_with('C') {
            // Rename / copy: current record is the NEW path, next record is
            // the OLD path — skip it.
            it.next();
        }
        if !path.is_empty() {
            paths.push(path.to_string());
        }
    }
    Ok(paths)
}

/// `true` when `path` is present in the index (i.e. `git ls-files` returns it).
/// Covers both files with an existing HEAD entry and freshly-`git add`ed files.
fn is_tracked(working_dir: &Path, path: &str) -> Result<bool> {
    let out = git_cmd(working_dir, &["ls-files", "--", path])?;
    Ok(!out.trim().is_empty())
}

/// `true` when `git check-ignore` marks `path` as ignored. `check-ignore`
/// uses exit 1 as "not ignored" (a normal signal, not an error), so this
/// bypasses [`git_cmd`] and inspects the exit status directly.
fn is_ignored(working_dir: &Path, path: &str) -> bool {
    let output = Command::new("git")
        .args(["check-ignore", "--quiet", "--", path])
        .current_dir(working_dir)
        .output();
    match output {
        Ok(o) => o.status.code() == Some(0),
        Err(_) => false,
    }
}

/// `git add -A -- <paths>`. Handles deletions in addition to
/// modifications / additions, unlike plain [`stage`].
fn stage_add_all(working_dir: &Path, paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut args = vec!["add", "-A", "--"];
    args.extend(paths.iter().map(|s| s.as_str()));
    git_cmd(working_dir, &args)?;
    Ok(())
}
