//! Read-only inspection: status, log, diff, worktree list.
//!
//! Everything here is safe to call without ownership checks — these methods
//! only observe the repository state.

use std::path::PathBuf;

use anyhow::Result;
use git2::{Repository, Status, StatusOptions};

use crate::output::{
    CommitEntry, DiffOutput, EntryStatus, LogOutput, StatusKind, StatusOutput, WorktreeEntry,
    WorktreeListOutput,
};
use crate::{GitModule, git_cmd};

impl GitModule {
    /// Inspect the working tree and produce a [`StatusOutput`] split into
    /// staged / unstaged / untracked buckets.
    pub fn status(&self) -> Result<StatusOutput> {
        let repo = Repository::open(self.session().root())?;

        let head_sha = match repo.head() {
            Ok(head) => head.target().map(|oid| oid.to_string()),
            Err(e) if e.code() == git2::ErrorCode::UnbornBranch => None,
            Err(e) => return Err(e.into()),
        };

        let branch = match repo.head() {
            Ok(head) => head.shorthand().map(|s| s.to_string()),
            Err(e) if e.code() == git2::ErrorCode::UnbornBranch => None,
            Err(e) => return Err(e.into()),
        };

        let mut opts = StatusOptions::new();
        opts.include_untracked(true).recurse_untracked_dirs(true);
        let statuses = repo.statuses(Some(&mut opts))?;

        let mut staged = Vec::new();
        let mut unstaged = Vec::new();
        let mut untracked = Vec::new();

        for entry in statuses.iter() {
            let path = match entry.path() {
                Some(p) => PathBuf::from(p),
                None => continue,
            };
            let st = entry.status();

            if st.intersects(Status::WT_NEW) {
                untracked.push(path.clone());
            }

            if let Some(kind) = staged_kind(st) {
                staged.push(EntryStatus {
                    path: path.clone(),
                    kind,
                });
            }

            if let Some(kind) = unstaged_kind(st) {
                unstaged.push(EntryStatus {
                    path: path.clone(),
                    kind,
                });
            }

            if st.intersects(Status::CONFLICTED) {
                staged.push(EntryStatus {
                    path,
                    kind: StatusKind::Conflicted,
                });
            }
        }

        let clean = staged.is_empty() && unstaged.is_empty() && untracked.is_empty();

        Ok(StatusOutput {
            branch,
            head_sha,
            staged,
            unstaged,
            untracked,
            clean,
        })
    }

    /// Walk HEAD back up to `max_count` commits.
    pub fn log(&self, max_count: usize) -> Result<LogOutput> {
        let repo = Repository::open(self.session().root())?;
        let mut revwalk = repo.revwalk()?;
        revwalk.push_head()?;
        let mut commits = Vec::new();
        for (i, oid) in revwalk.enumerate() {
            if i >= max_count {
                break;
            }
            let oid = oid?;
            let commit = repo.find_commit(oid)?;
            let sha = oid.to_string();
            let short_sha = sha[..7.min(sha.len())].to_string();
            let summary = commit.summary().unwrap_or("").to_string();
            commits.push(CommitEntry {
                sha,
                short_sha,
                summary,
            });
        }
        Ok(LogOutput { commits })
    }

    /// Produce the patch for either the staged (HEAD vs index) or the
    /// unstaged (index vs worktree) view, controlled by `staged`.
    pub fn diff(&self, staged: bool) -> Result<DiffOutput> {
        let repo = Repository::open(self.session().root())?;
        let head_tree = match repo.head() {
            Ok(head) => Some(head.peel_to_tree()?),
            Err(e) if e.code() == git2::ErrorCode::UnbornBranch => None,
            Err(e) => return Err(e.into()),
        };

        let diff = if staged {
            repo.diff_tree_to_index(head_tree.as_ref(), None, None)?
        } else {
            repo.diff_index_to_workdir(None, None)?
        };

        let file_count = diff.deltas().len();

        let mut patch = String::new();
        diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
            let origin = line.origin();
            if matches!(origin, '+' | '-' | ' ') {
                patch.push(origin);
            }
            patch.push_str(std::str::from_utf8(line.content()).unwrap_or(""));
            true
        })?;

        Ok(DiffOutput {
            staged,
            patch,
            file_count,
        })
    }

    /// List every linked worktree, annotated with session-ownership.
    pub fn worktree_list(&self) -> Result<WorktreeListOutput> {
        let raw = git_cmd(self.session().root(), &["worktree", "list", "--porcelain"])?;

        let mut worktrees = Vec::new();
        let mut current_path: Option<PathBuf> = None;
        let mut current_head: Option<String> = None;
        let mut current_branch: Option<String> = None;

        for line in raw.lines() {
            if line.is_empty() {
                if let Some(p) = current_path.take() {
                    worktrees.push(self.make_worktree_entry(
                        p,
                        current_head.take(),
                        current_branch.take(),
                    ));
                }
                continue;
            }
            if let Some(p) = line.strip_prefix("worktree ") {
                current_path = Some(PathBuf::from(p));
            } else if let Some(h) = line.strip_prefix("HEAD ") {
                current_head = Some(h.to_string());
            } else if let Some(b) = line.strip_prefix("branch ") {
                current_branch = Some(b.trim_start_matches("refs/heads/").to_string());
            }
        }
        if let Some(p) = current_path {
            worktrees.push(self.make_worktree_entry(p, current_head, current_branch));
        }

        Ok(WorktreeListOutput { worktrees })
    }

    fn make_worktree_entry(
        &self,
        path: PathBuf,
        head: Option<String>,
        branch: Option<String>,
    ) -> WorktreeEntry {
        let owned = self.owned_worktrees.contains(&path)
            || path
                .canonicalize()
                .map(|c| self.owned_worktrees.contains(&c))
                .unwrap_or(false);
        WorktreeEntry {
            path,
            head,
            branch,
            owned,
        }
    }
}

fn staged_kind(st: Status) -> Option<StatusKind> {
    if st.intersects(Status::INDEX_NEW) {
        Some(StatusKind::New)
    } else if st.intersects(Status::INDEX_MODIFIED) {
        Some(StatusKind::Modified)
    } else if st.intersects(Status::INDEX_DELETED) {
        Some(StatusKind::Deleted)
    } else if st.intersects(Status::INDEX_RENAMED) {
        Some(StatusKind::Renamed)
    } else if st.intersects(Status::INDEX_TYPECHANGE) {
        Some(StatusKind::Typechange)
    } else {
        None
    }
}

fn unstaged_kind(st: Status) -> Option<StatusKind> {
    if st.intersects(Status::WT_MODIFIED) {
        Some(StatusKind::Modified)
    } else if st.intersects(Status::WT_DELETED) {
        Some(StatusKind::Deleted)
    } else if st.intersects(Status::WT_RENAMED) {
        Some(StatusKind::Renamed)
    } else if st.intersects(Status::WT_TYPECHANGE) {
        Some(StatusKind::Typechange)
    } else {
        None
    }
}
