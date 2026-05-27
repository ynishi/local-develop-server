//! Git operations backed by [`git2`], with session-scoped write safety.
//!
//! Read operations (status, log, diff) are always available. Write
//! operations (commit, merge, worktree add/remove) are restricted to
//! worktrees created by the current session — preventing one agent from
//! destroying another's work.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use lds_core::Session;

/// Git module instance, tied to a [`Session`].
///
/// Tracks which worktrees were created by this session via
/// `owned_worktrees`. Write operations check ownership before
/// proceeding; read operations bypass the check entirely.
#[derive(Debug)]
pub struct GitModule {
    session: Arc<Session>,
    /// Worktrees created by this session — only these can be committed to / removed.
    owned_worktrees: HashSet<PathBuf>,
    /// Branches created by this session — only these can be deleted / merged.
    owned_branches: HashSet<String>,
}

impl GitModule {
    pub fn new(session: Arc<Session>) -> Self {
        Self {
            session,
            owned_worktrees: HashSet::new(),
            owned_branches: HashSet::new(),
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

    fn ensure_branch_owned(&self, branch: &str) -> Result<()> {
        if !self.owned_branches.contains(branch) {
            bail!(
                "branch not owned by this session ({}): {}",
                self.session.id(),
                branch,
            );
        }
        Ok(())
    }

    fn worktrees_dir(&self) -> PathBuf {
        self.session.root().join(".worktrees")
    }

    fn ensure_session_scope(&self, working_dir: &Path) -> Result<()> {
        if working_dir == self.session.root() {
            return Ok(());
        }
        let canon = working_dir
            .canonicalize()
            .unwrap_or_else(|_| working_dir.to_path_buf());
        if self.owned_worktrees.contains(&canon) {
            return Ok(());
        }
        if self.owned_worktrees.contains(working_dir) {
            return Ok(());
        }
        bail!(
            "working_dir not owned by this session ({}): {}",
            self.session.id(),
            working_dir.display(),
        );
    }

    // -- Write operations --

    pub fn worktree_list(&self) -> Result<String> {
        let out = git_cmd(self.session.root(), &["worktree", "list", "--porcelain"])?;
        let mut entries = Vec::new();
        let mut current: Vec<String> = Vec::new();
        for line in out.lines() {
            if line.is_empty() {
                if !current.is_empty() {
                    let entry_text = current.join("\n");
                    let path_line = current.first().and_then(|l| l.strip_prefix("worktree "));
                    let owned = path_line
                        .map(|p| {
                            let pb = PathBuf::from(p);
                            self.owned_worktrees.contains(&pb)
                        })
                        .unwrap_or(false);
                    entries.push(format!("{entry_text}\nowned: {owned}"));
                    current.clear();
                }
            } else {
                current.push(line.to_string());
            }
        }
        if !current.is_empty() {
            let entry_text = current.join("\n");
            let path_line = current.first().and_then(|l| l.strip_prefix("worktree "));
            let owned = path_line
                .map(|p| self.owned_worktrees.contains(&PathBuf::from(p)))
                .unwrap_or(false);
            entries.push(format!("{entry_text}\nowned: {owned}"));
        }
        Ok(entries.join("\n\n"))
    }

    pub fn worktree_add(
        &mut self,
        name: &str,
        branch: &str,
        base_branch: Option<&str>,
    ) -> Result<String> {
        let wt_dir = self.worktrees_dir();
        std::fs::create_dir_all(&wt_dir)
            .context("failed to create .worktrees directory")?;

        let wt_path = wt_dir.join(name);
        if wt_path.exists() {
            bail!("worktree already exists: {}", wt_path.display());
        }

        if let Some(base) = base_branch {
            git_cmd(
                self.session.root(),
                &["worktree", "add", "-b", branch, wt_path.to_str().unwrap_or(name), base],
            )?;
        } else {
            git_cmd(
                self.session.root(),
                &["worktree", "add", "-b", branch, wt_path.to_str().unwrap_or(name)],
            )?;
        }

        let canon = wt_path
            .canonicalize()
            .unwrap_or_else(|_| wt_path.clone());
        self.owned_worktrees.insert(canon);
        self.owned_branches.insert(branch.to_string());

        Ok(format!(
            "worktree created: path={}, branch={}, session={}",
            wt_path.display(),
            branch,
            self.session.id(),
        ))
    }

    pub fn worktree_remove(&mut self, name: &str) -> Result<String> {
        let wt_path = self.worktrees_dir().join(name);
        let canon = wt_path
            .canonicalize()
            .unwrap_or_else(|_| wt_path.clone());

        self.ensure_owned(&canon)
            .or_else(|_| self.ensure_owned(&wt_path))?;

        git_cmd(
            self.session.root(),
            &["worktree", "remove", "--force", wt_path.to_str().unwrap_or(name)],
        )?;

        self.owned_worktrees.remove(&canon);
        self.owned_worktrees.remove(&wt_path);

        Ok(format!("worktree removed: {}", wt_path.display()))
    }

    pub fn commit(
        &self,
        working_dir: &Path,
        message: &str,
        paths: Option<&[String]>,
    ) -> Result<String> {
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

        let hash = git_cmd(working_dir, &["rev-parse", "--short", "HEAD"])?;
        let files_changed = git_cmd(
            working_dir,
            &["diff", "--name-only", "HEAD~1..HEAD"],
        )
        .unwrap_or_default();

        let count = files_changed.lines().count();
        Ok(format!(
            "committed: hash={hash}, message={message}, files_changed={count}"
        ))
    }

    pub fn merge(
        &self,
        branch: &str,
        into_branch: &str,
        working_dir: &Path,
    ) -> Result<String> {
        self.ensure_session_scope(working_dir)?;

        let current = git_cmd(working_dir, &["branch", "--show-current"])?;
        if current != into_branch {
            git_cmd(working_dir, &["checkout", into_branch])?;
        }

        let result = git_cmd(working_dir, &["merge", "--no-ff", branch, "-m",
            &format!("Merge branch '{}' into {}", branch, into_branch)]);

        match result {
            Ok(out) => {
                let hash = git_cmd(working_dir, &["rev-parse", "--short", "HEAD"])?;
                Ok(format!("merged: {branch} -> {into_branch}, hash={hash}\n{out}"))
            }
            Err(e) => {
                let _ = git_cmd(working_dir, &["merge", "--abort"]);
                bail!("merge failed (aborted): {e}");
            }
        }
    }

    pub fn branch_delete(&self, branch: &str) -> Result<String> {
        self.ensure_branch_owned(branch)?;

        git_cmd(self.session.root(), &["branch", "-d", branch])?;
        Ok(format!("branch deleted: {branch}"))
    }
}

fn git_cmd(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("failed to run git {}", args.first().unwrap_or(&"")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {}: {}", args.first().unwrap_or(&""), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
