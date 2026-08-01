//! Stash as an explicit transaction, not a one-shot restore.
//!
//! The rest of this crate routes "park my work" through worktrees on
//! purpose, so a stash entry that shows up here is almost always a human's
//! own `git stash push`. The failure mode this module exists to prevent is
//! an agent doing `stash pop` → conflict → `reset --hard`, which throws that
//! work away leaving nothing to recover from. The surface is therefore split
//! so that no single call can both restore and discard:
//!
//! * [`GitModule::stash_apply`] restores content and *keeps* the entry. It
//!   refuses to run unless the working tree is clean, so the applied change
//!   is never mixed with hand edits and a rollback is always total.
//! * [`GitModule::stash_abort`] puts the touched paths back to HEAD.
//! * [`GitModule::stash_finalize`] drops the entry and reports its sha.
//!
//! Between apply and finalize there is room for an actual verification step
//! — that gap is the whole point.
//!
//! Even the drop is reversible for a while: [`GitModule::stash_restore`] puts
//! a finalized entry back on `refs/stash` from the `dropped_sha` that
//! `stash_finalize` returned, and stays usable until `git gc` prunes the now
//! unreferenced commit.
//!
//! **Permanently out of scope.** `stash push`, `stash pop`, a bare
//! `stash drop`, and `stash clear` are not offered and are not planned:
//!
//! * `push` — parking work belongs to [`GitModule::worktree_add`].
//! * `pop` — apply + drop fused together, with the drop landing *before*
//!   anyone can check the apply. Use `stash_apply` then `stash_finalize`.
//! * bare `drop` / `clear` — discard entries without surfacing what was
//!   discarded, i.e. exactly the "it's just gone" outcome above.
//!   `stash_finalize` is the supported drop and always returns
//!   [`StashFinalizeOutput::dropped_sha`].

use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};
use git2::{ObjectType, Oid, Repository, TreeWalkMode, TreeWalkResult};
use lds_core::Session;

use crate::output::{
    StashAbortOutput, StashApplyOutput, StashEntry, StashFinalizeOutput, StashListOutput,
    StashRestoreOutput, StashShowOutput,
};
use crate::read::blocking;
use crate::{GitModule, TIMEOUT_LOCAL, git_cmd, git_cmd_combined};

impl GitModule {
    /// List every stash entry, newest first.
    ///
    /// Read-only, so no ownership check — the entries belong to the
    /// repository, not to a session. Runs the libgit2 walk on the
    /// [`tokio::task::spawn_blocking`] pool.
    pub async fn stash_list(&self) -> Result<StashListOutput> {
        let session = Arc::clone(&self.session);
        blocking(move || stash_list_sync(&session)).await
    }

    /// Show what `stash@{index}` would restore: the patch against the commit
    /// the stash was taken on, plus the untracked paths it carries.
    ///
    /// Read-only; nothing is applied. Runs the libgit2 diff on the
    /// [`tokio::task::spawn_blocking`] pool.
    pub async fn stash_show(&self, index: usize) -> Result<StashShowOutput> {
        let session = Arc::clone(&self.session);
        blocking(move || stash_show_sync(&session, index)).await
    }

    /// Restore `stash@{index}` into `working_dir` **without dropping it**.
    ///
    /// Preconditions, all of them refusals rather than best-effort merges:
    ///
    /// 1. `expected_sha` (when given) must match the entry at `index`. The
    ///    index shifts on every drop, the sha does not — pass it whenever
    ///    the caller resolved the entry in an earlier turn.
    /// 2. `working_dir` must have no staged and no unstaged changes.
    ///    Untracked files are fine. This is what makes the rollback below
    ///    total: everything the working tree gains came from the stash, so
    ///    undoing the apply can never eat a hand edit.
    /// 3. None of the entry's untracked paths may already exist on disk —
    ///    git would refuse halfway through and leave a partial apply.
    ///
    /// On failure (conflict, or anything else `git stash apply` reports) the
    /// working tree is rolled back automatically and the error says so. The
    /// entry is never touched, so a failed apply costs nothing.
    pub async fn stash_apply(
        &self,
        working_dir: &Path,
        index: usize,
        expected_sha: Option<String>,
    ) -> Result<StashApplyOutput> {
        self.ensure_session_scope(working_dir)?;

        let entry = self.stash_entry_at(index).await?;
        ensure_sha_matches(&entry, expected_sha.as_deref())?;

        let (staged, unstaged) = dirty_paths(working_dir).await?;
        if !staged.is_empty() || !unstaged.is_empty() {
            bail!(
                "stash apply refused: working tree must be clean (staged: [{}], unstaged: [{}]). \
                 Commit the changes first, or park them with git_worktree_add — mixing hand edits \
                 with a stash apply makes an abort impossible to do safely. Untracked files are \
                 allowed.",
                staged.join(", "),
                unstaged.join(", "),
            );
        }

        let detail = self.stash_show(index).await?;
        let collisions: Vec<&String> = detail
            .untracked_paths
            .iter()
            .filter(|p| working_dir.join(p).exists())
            .collect();
        if !collisions.is_empty() {
            bail!(
                "stash apply refused: stash@{{{index}}} carries untracked files that already \
                 exist in the working tree: {}. Move or remove them first (git would abort \
                 mid-apply and leave the tree half-restored).",
                collisions
                    .iter()
                    .map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }

        let spec = stash_spec(index);
        if let Err(e) = git_cmd_combined(
            working_dir,
            &["stash", "apply", spec.as_str()],
            TIMEOUT_LOCAL,
        )
        .await
        {
            rollback_paths(working_dir, &detail.files, &detail.untracked_paths).await?;
            bail!(
                "stash apply failed and was rolled back (working tree is back at HEAD; \
                 stash@{{{index}}} sha={} is intact and nothing was dropped): {e}",
                entry.sha,
            );
        }

        Ok(StashApplyOutput {
            index,
            sha: entry.sha,
            applied_paths: detail.files,
            restored_untracked: detail.untracked_paths,
            entry_kept: true,
        })
    }

    /// Undo an applied stash: every path `stash@{index}` touches goes back to
    /// its HEAD state, and the entry itself is left alone.
    ///
    /// This is a path-scoped revert, not a diff-aware one — edits made on top
    /// of the applied stash are discarded together with it. That's the
    /// deliberate trade for [`GitModule::stash_apply`]'s clean-tree
    /// precondition: within that contract, "back to HEAD" and "undo the
    /// apply" are the same thing.
    pub async fn stash_abort(
        &self,
        working_dir: &Path,
        index: usize,
        expected_sha: Option<String>,
    ) -> Result<StashAbortOutput> {
        self.ensure_session_scope(working_dir)?;

        let entry = self.stash_entry_at(index).await?;
        ensure_sha_matches(&entry, expected_sha.as_deref())?;

        let detail = self.stash_show(index).await?;
        let report = rollback_paths(working_dir, &detail.files, &detail.untracked_paths).await?;

        Ok(StashAbortOutput {
            index,
            sha: entry.sha,
            reverted_paths: report.reverted,
            removed_untracked: report.removed_untracked,
            entry_kept: true,
        })
    }

    /// Drop `stash@{index}` once its content has been verified.
    ///
    /// The sha is read *before* the drop and returned as
    /// [`StashFinalizeOutput::dropped_sha`]: the stash commit stays reachable
    /// until `git gc` prunes it, so [`GitModule::stash_restore`] can put the
    /// entry back. Callers should record it — that record is the difference
    /// between "dropped" and "lost".
    pub async fn stash_finalize(
        &self,
        working_dir: &Path,
        index: usize,
        expected_sha: Option<String>,
    ) -> Result<StashFinalizeOutput> {
        self.ensure_session_scope(working_dir)?;

        let entry = self.stash_entry_at(index).await?;
        ensure_sha_matches(&entry, expected_sha.as_deref())?;

        let spec = stash_spec(index);
        git_cmd(
            working_dir,
            &["stash", "drop", spec.as_str()],
            TIMEOUT_LOCAL,
        )
        .await?;

        Ok(StashFinalizeOutput {
            index,
            dropped_sha: entry.sha,
            message: entry.message,
        })
    }

    /// Put a dropped stash commit back on `refs/stash`.
    ///
    /// `sha` is the [`StashFinalizeOutput::dropped_sha`] of an earlier
    /// finalize (or any stash commit sha). Dropping only removes the reflog
    /// entry — the commit itself lingers, unreferenced, until `git gc` prunes
    /// it, and this call re-references it.
    ///
    /// Guarded so `refs/stash` cannot be turned into a dumping ground:
    ///
    /// 1. `sha` must be >= 7 hex chars. Revspecs (`HEAD`, branch names,
    ///    `HEAD@{1}`) are rejected — restoring is only ever about an object id
    ///    somebody wrote down.
    /// 2. The object must resolve. When it doesn't, `git gc` is the likely
    ///    reason and the error says so.
    /// 3. The commit must be stash-shaped (>= 2 parents). Storing an ordinary
    ///    commit would produce an entry whose "apply" means something nobody
    ///    intended.
    /// 4. The entry must not already be in the list — a second reference to
    ///    the same content is a footgun, not a restore.
    ///
    /// `message` overrides the reflog message; when omitted the stash
    /// commit's own summary is reused, which is what the entry was called
    /// before it was dropped.
    pub async fn stash_restore(
        &self,
        working_dir: &Path,
        sha: &str,
        message: Option<String>,
    ) -> Result<StashRestoreOutput> {
        self.ensure_session_scope(working_dir)?;

        let sha = sha.trim().to_string();
        ensure_sha_shape(&sha)?;

        let session = Arc::clone(&self.session);
        let probe_sha = sha.clone();
        let probe = blocking(move || resolve_commit_sync(&session, &probe_sha)).await?;

        // A stash commit always has at least 2 parents (HEAD + index state),
        // plus a 3rd for the untracked snapshot.
        if probe.parent_count < 2 {
            bail!(
                "stash restore refused: {} is not a stash commit ({} parent(s); a stash commit \
                 has at least 2). Only shas produced by git_stash_finalize / git stash push can \
                 be restored.",
                probe.sha,
                probe.parent_count,
            );
        }

        let list = self.stash_list().await?;
        if let Some(existing) = list.stashes.iter().find(|e| e.sha == probe.sha) {
            bail!(
                "stash restore refused: {} is already present at stash@{{{}}} — restoring it \
                 again would put the same content in the list twice.",
                probe.sha,
                existing.index,
            );
        }

        let message = message
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
            .unwrap_or(probe.summary);

        git_cmd(
            working_dir,
            &["stash", "store", "-m", message.as_str(), probe.sha.as_str()],
            TIMEOUT_LOCAL,
        )
        .await?;

        Ok(StashRestoreOutput {
            restored_sha: probe.sha,
            index: 0,
            message,
        })
    }

    /// Resolve `stash@{index}` to its [`StashEntry`], erroring when the index
    /// is out of range (the common shape of "an entry was dropped under me").
    pub(crate) async fn stash_entry_at(&self, index: usize) -> Result<StashEntry> {
        let list = self.stash_list().await?;
        let total = list.stashes.len();
        list.stashes
            .into_iter()
            .find(|e| e.index == index)
            .ok_or_else(|| {
                anyhow::anyhow!("no stash entry at index {index} ({total} entr(y|ies) present)")
            })
    }
}

/// `stash@{N}` — kept in one place because the brace escaping is easy to get
/// wrong in a `format!`.
fn stash_spec(index: usize) -> String {
    format!("stash@{{{index}}}")
}

/// Verify that the entry the caller *thinks* they are acting on is the entry
/// that currently sits at that index. A prefix of at least 7 chars is
/// accepted, matching git's own short-sha convention.
fn ensure_sha_matches(entry: &StashEntry, expected: Option<&str>) -> Result<()> {
    let Some(expected) = expected.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    if expected.len() < 7 {
        bail!("expected_sha {expected:?} is too short (need at least 7 hex chars)");
    }
    if !entry.sha.starts_with(expected) {
        bail!(
            "stash index shifted: stash@{{{}}} is now {} (expected {expected}). \
             Re-read git_stash_list and retry with the current index.",
            entry.index,
            entry.sha,
        );
    }
    Ok(())
}

/// Accept only what an object id can look like: >= 7 hex chars, nothing else.
///
/// Rejecting revspecs (`HEAD`, `main`, `HEAD@{1}`) matters because
/// [`GitModule::stash_restore`] feeds this straight into `revparse_single` —
/// a caller who passes a branch name means something the restore path cannot
/// honour, and silently resolving it would store a non-stash commit.
fn ensure_sha_shape(sha: &str) -> Result<()> {
    if sha.len() < 7 {
        bail!("sha {sha:?} is too short (need at least 7 hex chars)");
    }
    if !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!(
            "sha {sha:?} is not an object id — revspecs (HEAD, branch names, HEAD@{{1}}) are \
             rejected here on purpose; pass the dropped_sha reported by git_stash_finalize."
        );
    }
    Ok(())
}

/// `(staged, unstaged)` path lists for `working_dir`.
///
/// Uses `git diff --name-only` rather than `--porcelain` because
/// [`git_cmd`] trims stdout, which would eat the leading status column of a
/// ` M path` line. Untracked files are deliberately absent: they don't
/// conflict with an apply unless the stash carries the same path, which
/// [`GitModule::stash_apply`] checks separately.
pub(crate) async fn dirty_paths(working_dir: &Path) -> Result<(Vec<String>, Vec<String>)> {
    let staged = git_cmd(
        working_dir,
        &["diff", "--cached", "--name-only", "-z"],
        TIMEOUT_LOCAL,
    )
    .await?;
    let unstaged = git_cmd(working_dir, &["diff", "--name-only", "-z"], TIMEOUT_LOCAL).await?;
    Ok((split_nul(&staged), split_nul(&unstaged)))
}

/// What [`rollback_paths`] actually did, so callers can report it verbatim.
struct RollbackReport {
    /// Tracked paths returned to their HEAD state.
    reverted: Vec<String>,
    /// Untracked paths removed from the working tree.
    removed_untracked: Vec<String>,
}

/// Return the paths a stash entry touches to their HEAD state.
///
/// Safe as a *whole-tree* rollback only because
/// [`GitModule::stash_apply`] guarantees the tree was clean beforehand —
/// there is nothing else in those paths that could be destroyed.
async fn rollback_paths(
    working_dir: &Path,
    tracked: &[String],
    untracked: &[String],
) -> Result<RollbackReport> {
    // Drop whatever the apply put in the index — including the unmerged
    // entries a conflict leaves behind, which would otherwise block the
    // checkout below.
    git_cmd(working_dir, &["reset", "--mixed", "HEAD"], TIMEOUT_LOCAL).await?;

    let in_head = paths_in_head(working_dir, tracked).await?;
    if !in_head.is_empty() {
        let mut args = vec!["checkout", "-f", "HEAD", "--"];
        args.extend(in_head.iter().map(|s| s.as_str()));
        git_cmd(working_dir, &args, TIMEOUT_LOCAL).await?;
    }

    // Paths the entry *added* have no HEAD state to restore. After the
    // `reset --mixed` they are plain untracked files, so removing them is
    // the whole job.
    for path in tracked.iter().filter(|p| !in_head.contains(p)) {
        remove_worktree_file(working_dir, path);
    }

    let mut removed_untracked = Vec::new();
    for path in untracked {
        if working_dir.join(path).exists() {
            remove_worktree_file(working_dir, path);
            removed_untracked.push(path.clone());
        }
    }

    Ok(RollbackReport {
        reverted: tracked.to_vec(),
        removed_untracked,
    })
}

/// Subset of `paths` that exists in the HEAD tree.
async fn paths_in_head(working_dir: &Path, paths: &[String]) -> Result<Vec<String>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = vec!["ls-tree", "-r", "-z", "--name-only", "HEAD", "--"];
    args.extend(paths.iter().map(|s| s.as_str()));
    let raw = git_cmd(working_dir, &args, TIMEOUT_LOCAL).await?;
    Ok(split_nul(&raw))
}

/// Best-effort worktree file removal. A missing file is the desired end
/// state, so `NotFound` is success; anything else is warned about rather
/// than propagated — the caller is already in a rollback path and a partial
/// cleanup must not mask the original error.
fn remove_worktree_file(working_dir: &Path, rel_path: &str) {
    let path = working_dir.join(rel_path);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "stash rollback: remove failed");
        }
    }
}

/// Split nul-delimited git output (`-z`), dropping the trailing empty record.
fn split_nul(raw: &str) -> Vec<String> {
    raw.split('\0')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn stash_list_sync(session: &Session) -> Result<StashListOutput> {
    let mut repo = Repository::open(session.root())?;
    let raw = collect_stash_refs(&mut repo)?;

    let mut stashes = Vec::with_capacity(raw.len());
    for (index, message, oid) in raw {
        let commit = repo.find_commit(oid)?;
        stashes.push(StashEntry {
            index,
            sha: oid.to_string(),
            message,
            // A stash commit has 2 parents normally (HEAD + index state) and
            // a 3rd holding the untracked snapshot when pushed with `-u`.
            has_untracked: commit.parent_count() >= 3,
        });
    }
    Ok(StashListOutput { stashes })
}

fn stash_show_sync(session: &Session, index: usize) -> Result<StashShowOutput> {
    let mut repo = Repository::open(session.root())?;
    let raw = collect_stash_refs(&mut repo)?;
    let total = raw.len();
    let (_, message, oid) = raw
        .into_iter()
        .find(|(i, _, _)| *i == index)
        .ok_or_else(|| anyhow::anyhow!("no stash entry at index {index} ({total} present)"))?;

    let commit = repo.find_commit(oid)?;
    let stash_tree = commit.tree()?;
    // parent(0) is the commit HEAD pointed at when the stash was created —
    // diffing against it is what `git stash show -p` reports.
    let base_tree = commit.parent(0)?.tree()?;

    let diff = repo.diff_tree_to_tree(Some(&base_tree), Some(&stash_tree), None)?;
    let file_count = diff.deltas().len();

    let mut files = Vec::with_capacity(file_count);
    for delta in diff.deltas() {
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .map(|p| p.to_string_lossy().to_string());
        if let Some(path) = path {
            files.push(path);
        }
    }

    let mut patch = String::new();
    diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
        let origin = line.origin();
        if matches!(origin, '+' | '-' | ' ') {
            patch.push(origin);
        }
        patch.push_str(std::str::from_utf8(line.content()).unwrap_or(""));
        true
    })?;

    // The untracked snapshot is a standalone commit in the 3rd parent slot;
    // its tree *is* the untracked file set, so a plain walk enumerates it.
    let untracked_paths = if commit.parent_count() >= 3 {
        collect_tree_paths(&commit.parent(2)?.tree()?)?
    } else {
        Vec::new()
    };

    Ok(StashShowOutput {
        index,
        sha: oid.to_string(),
        message,
        patch,
        file_count,
        files,
        untracked_paths,
    })
}

/// What [`GitModule::stash_restore`] needs to know about a candidate commit
/// before it is allowed near `refs/stash`.
struct CommitProbe {
    /// Full 40-char sha (the caller may have passed a prefix).
    sha: String,
    /// First line of the commit message — the reflog message a stash entry
    /// carried before it was dropped.
    summary: String,
    parent_count: usize,
}

/// Resolve `sha` to a commit without touching any ref.
///
/// A dropped stash commit is unreferenced but still in the object database,
/// which is exactly what makes restore possible — and what makes `git gc` the
/// deadline.
fn resolve_commit_sync(session: &Session, sha: &str) -> Result<CommitProbe> {
    let repo = Repository::open(session.root())?;
    let object = repo.revparse_single(sha).map_err(|e| {
        anyhow::anyhow!(
            "cannot resolve {sha}: {e}. A dropped stash commit stays in the object database \
             only until `git gc` prunes it — if gc has run since the drop, the content is gone."
        )
    })?;
    let commit = object
        .peel_to_commit()
        .map_err(|e| anyhow::anyhow!("{sha} does not resolve to a commit: {e}"))?;

    Ok(CommitProbe {
        sha: commit.id().to_string(),
        summary: commit.summary().unwrap_or_default().to_string(),
        parent_count: commit.parent_count(),
    })
}

/// `(index, message, oid)` for every stash entry, in reflog order.
///
/// `stash_foreach` needs `&mut Repository`, so the tuples are collected
/// eagerly and the borrow released before the caller inspects commits.
fn collect_stash_refs(repo: &mut Repository) -> Result<Vec<(usize, String, Oid)>> {
    let mut out = Vec::new();
    repo.stash_foreach(|index, message, oid| {
        out.push((index, message.to_string(), *oid));
        true
    })?;
    Ok(out)
}

/// Every blob path in `tree`, repo-relative.
fn collect_tree_paths(tree: &git2::Tree<'_>) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    tree.walk(TreeWalkMode::PreOrder, |root, entry| {
        if entry.kind() == Some(ObjectType::Blob) {
            // `root` is "" at the top level and "dir/" below it.
            paths.push(format!("{root}{}", entry.name().unwrap_or_default()));
        }
        TreeWalkResult::Ok
    })?;
    Ok(paths)
}
