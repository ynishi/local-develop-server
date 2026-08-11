//! Typed return shapes for [`GitModule`] methods.
//!
//! Every public method on [`GitModule`] returns one of these structs (wrapped
//! in [`anyhow::Result`]) instead of a `format!`-shaped `String`. The lds MCP
//! layer then serialises the struct with `serde_json::to_string_pretty` so
//! callers receive a stable JSON shape and can access fields directly.
//!
//! Keep this module field-stable: any rename / type change is a wire breakage
//! and must be paired with a SemVer bump on the lds-git crate and the lds MCP
//! tool description.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

/// One entry from `git status` (a single path with its current state).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EntryStatus {
    pub path: PathBuf,
    pub kind: StatusKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StatusKind {
    New,
    Modified,
    Deleted,
    Renamed,
    Typechange,
    Conflicted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusOutput {
    /// Current branch name (HEAD short name), `None` on detached HEAD.
    pub branch: Option<String>,
    /// HEAD commit sha (full 40-char hex), `None` for an unborn HEAD.
    pub head_sha: Option<String>,
    /// Entries with staged changes (index vs HEAD).
    pub staged: Vec<EntryStatus>,
    /// Entries with unstaged changes (worktree vs index).
    pub unstaged: Vec<EntryStatus>,
    /// Paths git reports as untracked (worktree-only files).
    pub untracked: Vec<PathBuf>,
    /// `true` when staged + unstaged + untracked are all empty.
    pub clean: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommitEntry {
    /// Full 40-char commit sha.
    pub sha: String,
    /// First 7 chars of `sha` (git's conventional short form).
    pub short_sha: String,
    /// First line of the commit message.
    pub summary: String,
    /// Commit author in `"Name <email>"` form.
    pub author: String,
    /// Commit author time, unix epoch seconds.
    pub timestamp: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogOutput {
    pub commits: Vec<CommitEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiffOutput {
    /// `true` when the diff is `git diff --cached` (HEAD vs index);
    /// `false` when it's `git diff` (index vs worktree).
    pub staged: bool,
    /// Unified diff patch, byte-for-byte equivalent to `git diff [--cached]`.
    pub patch: String,
    /// Number of distinct files touched by the diff.
    pub file_count: usize,
}

// ---------------------------------------------------------------------------
// Worktree
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    /// HEAD commit sha of this worktree (full 40-char hex), if any.
    pub head: Option<String>,
    /// Checked-out branch (short ref name), `None` on detached HEAD.
    pub branch: Option<String>,
    /// `true` when this worktree was created by the current session.
    pub owned: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorktreeListOutput {
    pub worktrees: Vec<WorktreeEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorktreeStateOutput {
    /// Branch name (short ref).
    pub branch: String,
    /// Upstream tracking branch (e.g. `origin/main`), `None` when unset.
    pub tracking: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// Number of uncommitted changes (staged + unstaged + untracked).
    pub uncommitted: usize,
    pub clean: bool,
    /// `true` when `behind == 0` (no incoming work to integrate).
    pub sync: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorktreeAddOutput {
    pub path: PathBuf,
    pub branch: String,
    pub session: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorktreeRemoveOutput {
    pub path: PathBuf,
}

// ---------------------------------------------------------------------------
// Remote
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FetchOutput {
    pub remote: String,
    pub refspec: Option<String>,
    /// `true` when `--prune` was requested.
    pub prune: bool,
    /// Raw transport output (stdout merged with stderr) for diagnostics.
    pub raw: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteEntry {
    pub name: String,
    pub fetch_url: Option<String>,
    pub push_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteListOutput {
    pub remotes: Vec<RemoteEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BranchStatusOutput {
    pub branch: String,
    pub base: String,
    pub ahead: u32,
    pub behind: u32,
    pub up_to_date: bool,
    /// Merge-base sha (full 40-char hex), `None` when no common ancestor.
    pub common_ancestor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UnpushedCommitsOutput {
    pub branch: String,
    pub remote: String,
    /// Sha of the remote tracking ref's tip (`<remote>/<branch>`).
    pub remote_head: String,
    pub count: usize,
    pub commits: Vec<CommitEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IsPushedOutput {
    pub commit: String,
    pub remote: String,
    pub pushed: bool,
    /// Remote refs that contain this commit (e.g. `refs/remotes/origin/main`).
    pub refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TagPushedOutput {
    pub tag: String,
    pub remote: String,
    pub pushed: bool,
    /// Raw lines from `git ls-remote --tags <remote> refs/tags/<tag>`.
    pub remote_refs: Vec<String>,
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommitOutput {
    pub sha: String,
    pub short_sha: String,
    pub message: String,
    pub files_changed: usize,
    /// Dotfile / dot-dir paths that surfaced during this commit call. Every
    /// change with a `.`-prefixed path component (`.env`,
    /// `.github/workflows/ci.yml`, `foo/.hidden`) lands here so pre-publish
    /// eyeballing can catch unintended edits. Tracked entries were still
    /// committed; untracked-not-in-gitignore entries were skipped (see
    /// `dotfile_skipped`). `force_dot=true` suppresses this entirely.
    #[serde(default)]
    pub dotfile_warnings: Vec<DotfileWarning>,
    /// Paths dropped from staging by the dotfile safeguard. Populated only
    /// for the untracked + not-in-gitignore branch (silent-ignored dotfiles
    /// aren't tracked here — git's default already handles them).
    #[serde(default)]
    pub dotfile_skipped: Vec<String>,
}

/// One dotfile / dot-dir path observed during commit. `tracked=true` means
/// the change was still committed (with a warn); `tracked=false` means it
/// was skipped from staging.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DotfileWarning {
    pub path: String,
    pub tracked: bool,
    pub in_gitignore: bool,
}

/// How [`GitModule::commit`] handles staged paths outside the `only` list.
///
/// Only consulted when `only` is `Some(non_empty)`. When `only` is `None` /
/// empty, `commit` still stages every change via `git add -A` and this enum
/// is ignored.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OtherStagedMode {
    /// Fail (state unchanged) when the index carries paths outside `only`.
    /// The safe default — protects against silently sweeping unrelated work.
    #[default]
    Stop,
    /// Unstage the other paths, commit `only`, then re-stage them. The
    /// commit ends up containing exactly the `only` paths; the pre-existing
    /// staged work stays in the index afterwards.
    Restage,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MergeOutput {
    pub branch: String,
    pub into_branch: String,
    /// Merge commit sha (full 40-char hex).
    pub sha: String,
    pub short_sha: String,
    /// Raw `git merge` output for diagnostics (fast-forward note, conflict tip).
    pub raw: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BranchDeleteOutput {
    pub branch: String,
}

// ---------------------------------------------------------------------------
// Reset
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResetOutput {
    pub mode: ResetMode,
    /// Revspec / sha that was passed in as the reset target.
    pub target: String,
    /// HEAD sha before the reset (full 40-char hex).
    pub previous_head: String,
    /// HEAD sha after the reset (full 40-char hex).
    pub current_head: String,
}

// ---------------------------------------------------------------------------
// Stash
// ---------------------------------------------------------------------------

/// One entry from `git stash list`.
///
/// `index` is the position in the stash reflog (`stash@{index}`) at the time
/// of the call — it shifts whenever an entry is dropped, which is why every
/// mutating stash method accepts an `expected_sha` to pin the identity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StashEntry {
    /// Position in the stash reflog (`stash@{index}`).
    pub index: usize,
    /// Stash commit sha (full 40-char hex). Stable across index shifts.
    pub sha: String,
    /// Reflog message (e.g. `"WIP on main: 1a2b3c4 subject"`).
    pub message: String,
    /// `true` when the entry carries untracked files (`git stash push -u`),
    /// detected via the stash commit's third parent.
    pub has_untracked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StashListOutput {
    /// Entries in reflog order (index 0 == most recent).
    pub stashes: Vec<StashEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StashShowOutput {
    pub index: usize,
    /// Stash commit sha (full 40-char hex).
    pub sha: String,
    /// Reflog message of the entry.
    pub message: String,
    /// Unified diff of the stashed tracked changes (base commit vs stash).
    pub patch: String,
    /// Number of tracked files the entry touches.
    pub file_count: usize,
    /// Repo-relative paths (git pathspec form) of the tracked files above.
    pub files: Vec<String>,
    /// Repo-relative paths carried as untracked files (`git stash push -u`).
    /// Empty when `has_untracked` is `false`; no patch is produced for them.
    pub untracked_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StashApplyOutput {
    pub index: usize,
    /// Stash commit sha (full 40-char hex) that was applied.
    pub sha: String,
    /// Tracked paths the entry restored into the working tree.
    pub applied_paths: Vec<String>,
    /// Untracked paths the entry restored into the working tree.
    pub restored_untracked: Vec<String>,
    /// Always `true` — apply never drops the entry. Dropping is
    /// [`super::GitModule::stash_finalize`]'s job.
    pub entry_kept: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StashAbortOutput {
    pub index: usize,
    /// Stash commit sha (full 40-char hex) whose apply was rolled back.
    pub sha: String,
    /// Tracked paths returned to their HEAD state (restored or, when the
    /// entry added them, removed from the working tree).
    pub reverted_paths: Vec<String>,
    /// Untracked paths removed from the working tree.
    pub removed_untracked: Vec<String>,
    /// Always `true` — abort undoes the apply, it does not drop the entry.
    pub entry_kept: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StashFinalizeOutput {
    /// Index the entry occupied before the drop.
    pub index: usize,
    /// Sha of the dropped stash commit (full 40-char hex). The commit itself
    /// survives until `git gc` prunes it, so this is the recovery key: feed
    /// it to [`super::GitModule::stash_restore`] to put the entry back.
    pub dropped_sha: String,
    /// Reflog message of the dropped entry.
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StashRestoreOutput {
    /// Full 40-char sha of the stash commit now back on `refs/stash`.
    pub restored_sha: String,
    /// Always `0` — `git stash store` pushes onto the top of the list, so the
    /// restored entry is `stash@{0}` and every pre-existing index shifts by 1.
    pub index: usize,
    /// Reflog message the restored entry carries (the caller's `message`, or
    /// the stash commit's own summary when none was given).
    pub message: String,
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionReleaseOutput {
    /// Worktree paths whose ownership this session adopted.
    pub adopted_worktrees: Vec<PathBuf>,
    /// Branches whose ownership this session adopted.
    pub adopted_branches: Vec<String>,
}
