//! Read-only inspection: status, log, diff, worktree list.
//!
//! Everything here is safe to call without ownership checks — these methods
//! only observe the repository state.
//!
//! libgit2 (`Repository::open` and every operation reachable from it) is a
//! synchronous C library — a `Repository` handle is not `Send`, and any call
//! that walks refs / statuses / diffs will block the calling thread on FS
//! reads. We therefore run every libgit2 fragment inside
//! [`tokio::task::spawn_blocking`] so it never occupies an async worker
//! thread; parallel MCP tool calls stay responsive even when one caller is
//! chewing through a large repository on a slow disk.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use git2::{DiffOptions, Repository, Status, StatusOptions};
use lds_core::Session;

use crate::output::{
    CommitEntry, DiffOutput, EntryStatus, LogOutput, StatusKind, StatusOutput, WorktreeEntry,
    WorktreeListOutput,
};
use crate::{GitModule, TIMEOUT_LOCAL, git_cmd};

/// Optional narrowing for [`GitModule::log`]. All fields are AND-combined; an
/// absent field imposes no constraint.
#[derive(Debug, Clone, Default)]
pub struct LogFilters {
    /// Cap on how many matching commits to return (post-filter).
    pub max_count: usize,
    /// Case-sensitive substring match against `"Name <email>"`.
    pub author: Option<String>,
    /// Git pathspec entries; a commit is kept when its diff against its first
    /// parent (or against the empty tree for the root commit) touches at least
    /// one entry. Empty vec is treated as "no path filter".
    pub paths: Option<Vec<String>>,
    /// Keep only commits whose author time is `>=` this value (unix epoch
    /// seconds).
    pub since: Option<i64>,
    /// Case-sensitive substring match against the full commit message
    /// (subject + body).
    pub grep: Option<String>,
}

impl GitModule {
    /// Inspect the working tree and produce a [`StatusOutput`] split into
    /// staged / unstaged / untracked buckets.
    ///
    /// Runs the libgit2 work on the [`tokio::task::spawn_blocking`] pool.
    pub async fn status(&self) -> Result<StatusOutput> {
        let session = Arc::clone(&self.session);
        blocking(move || status_sync(&session)).await
    }

    /// Walk HEAD backwards, applying [`LogFilters`] and returning at most
    /// `filters.max_count` matching commits.
    ///
    /// Runs the libgit2 revwalk on the [`tokio::task::spawn_blocking`] pool.
    pub async fn log(&self, filters: LogFilters) -> Result<LogOutput> {
        let session = Arc::clone(&self.session);
        blocking(move || log_sync(&session, filters)).await
    }

    /// Produce the patch for either the staged (HEAD vs index) or the
    /// unstaged (index vs worktree) view, controlled by `staged`.
    ///
    /// Runs the libgit2 diff on the [`tokio::task::spawn_blocking`] pool.
    pub async fn diff(&self, staged: bool) -> Result<DiffOutput> {
        let session = Arc::clone(&self.session);
        blocking(move || diff_sync(&session, staged)).await
    }

    /// List every linked worktree, annotated with session-ownership.
    pub async fn worktree_list(&self) -> Result<WorktreeListOutput> {
        let raw = git_cmd(
            self.session().root(),
            &["worktree", "list", "--porcelain"],
            TIMEOUT_LOCAL,
        )
        .await?;

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

/// Run `f` on the [`tokio::task::spawn_blocking`] pool and unwrap the
/// [`tokio::task::JoinError`] with a stable context message. The inner
/// `Result` is preserved verbatim.
pub(crate) async fn blocking<F, T>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .context("spawn_blocking join failed")?
}

fn status_sync(session: &Session) -> Result<StatusOutput> {
    let repo = Repository::open(session.root())?;

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

fn log_sync(session: &Session, filters: LogFilters) -> Result<LogOutput> {
    let repo = Repository::open(session.root())?;
    let mut revwalk = repo.revwalk()?;
    revwalk.push_head()?;

    let path_specs = filters
        .paths
        .as_ref()
        .and_then(|p| if p.is_empty() { None } else { Some(p.clone()) });

    let mut commits = Vec::new();
    for oid in revwalk {
        if commits.len() >= filters.max_count {
            break;
        }
        let oid = oid?;
        let commit = repo.find_commit(oid)?;

        if let Some(min_ts) = filters.since
            && commit.time().seconds() < min_ts
        {
            continue;
        }

        if let Some(needle) = &filters.author {
            let name = commit.author().name().unwrap_or("").to_string();
            let email = commit.author().email().unwrap_or("").to_string();
            let full = format!("{name} <{email}>");
            if !full.contains(needle.as_str()) {
                continue;
            }
        }

        if let Some(needle) = &filters.grep {
            let message = commit.message().unwrap_or("");
            if !message.contains(needle.as_str()) {
                continue;
            }
        }

        if let Some(specs) = &path_specs {
            let commit_tree = commit.tree()?;
            let parent_tree = if commit.parent_count() > 0 {
                Some(commit.parent(0)?.tree()?)
            } else {
                None
            };
            let mut diff_opts = DiffOptions::new();
            for spec in specs {
                diff_opts.pathspec(spec);
            }
            let diff = repo.diff_tree_to_tree(
                parent_tree.as_ref(),
                Some(&commit_tree),
                Some(&mut diff_opts),
            )?;
            if diff.deltas().len() == 0 {
                continue;
            }
        }

        let sha = oid.to_string();
        let short_sha = sha[..7.min(sha.len())].to_string();
        let summary = commit.summary().unwrap_or("").to_string();
        let author_name = commit.author().name().unwrap_or("").to_string();
        let author_email = commit.author().email().unwrap_or("").to_string();
        let author = format!("{author_name} <{author_email}>");
        let timestamp = commit.time().seconds();
        commits.push(CommitEntry {
            sha,
            short_sha,
            summary,
            author,
            timestamp,
        });
    }
    Ok(LogOutput { commits })
}

fn diff_sync(session: &Session, staged: bool) -> Result<DiffOutput> {
    let repo = Repository::open(session.root())?;
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
