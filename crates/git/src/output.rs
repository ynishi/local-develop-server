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
// Session
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionReleaseOutput {
    /// Worktree paths whose ownership this session adopted.
    pub adopted_worktrees: Vec<PathBuf>,
    /// Branches whose ownership this session adopted.
    pub adopted_branches: Vec<String>,
}
