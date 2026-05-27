use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Result};
use lds_core::Session;

/// Git module with write-scope tracking.
/// Read operations (log/diff/status) are always available.
/// Write operations (commit/merge/worktree_add/remove) are scoped
/// to worktrees created by this session.
#[derive(Debug)]
pub struct GitModule {
    session: Arc<Session>,
    /// Worktrees created by this session — only these can be committed to / removed.
    owned_worktrees: HashSet<PathBuf>,
}

impl GitModule {
    pub fn new(session: Arc<Session>) -> Self {
        Self {
            session,
            owned_worktrees: HashSet::new(),
        }
    }

    // -- Read operations (no scope check) --

    pub fn status(&self) -> Result<String> {
        let repo = git2::Repository::open(self.session.root())?;
        let statuses = repo.statuses(None)?;
        let mut out = String::new();
        for entry in statuses.iter() {
            let path = entry.path().unwrap_or("???");
            let st = entry.status();
            out.push_str(&format!("{st:?} {path}\n"));
        }
        Ok(out)
    }

    pub fn log(&self, max_count: usize) -> Result<String> {
        let repo = git2::Repository::open(self.session.root())?;
        let mut revwalk = repo.revwalk()?;
        revwalk.push_head()?;
        let mut out = String::new();
        for (i, oid) in revwalk.enumerate() {
            if i >= max_count {
                break;
            }
            let oid = oid?;
            let commit = repo.find_commit(oid)?;
            let summary = commit.summary().unwrap_or("");
            out.push_str(&format!("{} {summary}\n", &oid.to_string()[..7]));
        }
        Ok(out)
    }

    pub fn diff(&self) -> Result<String> {
        let repo = git2::Repository::open(self.session.root())?;
        let head = repo.head()?.peel_to_tree()?;
        let diff = repo.diff_tree_to_workdir_with_index(Some(&head), None)?;
        let mut out = String::new();
        diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
            let origin = line.origin();
            if matches!(origin, '+' | '-' | ' ') {
                out.push(origin);
            }
            out.push_str(std::str::from_utf8(line.content()).unwrap_or(""));
            true
        })?;
        Ok(out)
    }

    // -- Write operations (scope-checked) --

    pub fn register_worktree(&mut self, path: PathBuf) {
        self.owned_worktrees.insert(path);
    }

    pub fn is_owned(&self, path: &PathBuf) -> bool {
        self.owned_worktrees.contains(path)
    }

    pub fn ensure_owned(&self, path: &PathBuf) -> Result<()> {
        if !self.is_owned(path) {
            bail!(
                "worktree not owned by this session ({}): {}",
                self.session.id(),
                path.display()
            );
        }
        Ok(())
    }
}
