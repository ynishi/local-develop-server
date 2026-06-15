//! Remote-tracking inspection: fetch, remote list, branch / worktree state
//! relative to upstream, push reachability.
//!
//! These all delegate to `git` over a subprocess (via [`git_cmd`] /
//! [`git_cmd_combined`]) rather than the git2 C bindings — `git fetch` /
//! `git ls-remote` need access to the user's credential helpers and refspecs,
//! which git2 only exposes through `RemoteCallbacks` that we'd have to wire
//! up to the surrounding session. Shelling out keeps the contract identical
//! to what the user would type at a shell.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use git2::Repository;

use crate::output::{
    BranchStatusOutput, CommitEntry, FetchOutput, IsPushedOutput, RemoteEntry, RemoteListOutput,
    TagPushedOutput, UnpushedCommitsOutput, WorktreeStateOutput,
};
use crate::{GitModule, git_cmd, git_cmd_combined};

impl GitModule {
    /// Fetch from a remote.
    ///
    /// `remote` defaults to `"origin"`. `refspec` is forwarded verbatim when
    /// present. `prune` adds `--prune` so deleted upstream refs are removed
    /// locally. The combined stdout + stderr is returned as `raw` for
    /// transport diagnostics — successful fetches usually print nothing on
    /// stdout but emit `From <url>` / `<ref> -> <ref>` lines on stderr.
    pub fn fetch(
        &self,
        remote: Option<&str>,
        refspec: Option<&str>,
        prune: bool,
    ) -> Result<FetchOutput> {
        let remote = remote.unwrap_or("origin").to_string();
        let mut args: Vec<&str> = vec!["fetch"];
        if prune {
            args.push("--prune");
        }
        args.push(&remote);
        if let Some(rs) = refspec {
            args.push(rs);
        }
        let raw = git_cmd_combined(self.session().root(), &args)?;
        Ok(FetchOutput {
            remote,
            refspec: refspec.map(|s| s.to_string()),
            prune,
            raw,
        })
    }

    /// Enumerate remotes with their fetch / push URLs.
    ///
    /// We use `git remote -v` rather than git2's `Repository::remotes` so the
    /// output matches what `git` itself reports (and naturally handles the
    /// case where fetch and push URLs diverge via `pushurl`).
    pub fn remote_list(&self) -> Result<RemoteListOutput> {
        let raw = git_cmd(self.session().root(), &["remote", "-v"])?;
        let mut by_name: BTreeMap<String, RemoteEntry> = BTreeMap::new();
        for line in raw.lines() {
            // Format: "<name>\t<url> (fetch)" or "<name>\t<url> (push)"
            let (name, rest) = match line.split_once('\t') {
                Some(parts) => parts,
                None => continue,
            };
            let (url, dir) = match rest.rsplit_once(' ') {
                Some((u, d)) => (u.trim(), d.trim()),
                None => continue,
            };
            let entry = by_name
                .entry(name.to_string())
                .or_insert_with(|| RemoteEntry {
                    name: name.to_string(),
                    fetch_url: None,
                    push_url: None,
                });
            match dir {
                "(fetch)" => entry.fetch_url = Some(url.to_string()),
                "(push)" => entry.push_url = Some(url.to_string()),
                _ => {}
            }
        }
        Ok(RemoteListOutput {
            remotes: by_name.into_values().collect(),
        })
    }

    /// Compare `branch` against `base` and report ahead / behind counts plus
    /// the merge-base, using git2's `graph_ahead_behind` for the count and a
    /// `git merge-base` shell-out for the common-ancestor sha (git2's
    /// `merge_base` returns an OID but we want the textual form).
    pub fn branch_status(&self, branch: &str, base: &str) -> Result<BranchStatusOutput> {
        let repo = Repository::open(self.session().root())?;
        let branch_oid = repo
            .revparse_single(branch)
            .with_context(|| format!("revparse failed: {branch}"))?
            .id();
        let base_oid = repo
            .revparse_single(base)
            .with_context(|| format!("revparse failed: {base}"))?
            .id();
        let (ahead, behind) = repo
            .graph_ahead_behind(branch_oid, base_oid)
            .with_context(|| format!("graph_ahead_behind({branch}, {base})"))?;

        let common_ancestor = repo
            .merge_base(branch_oid, base_oid)
            .ok()
            .map(|oid| oid.to_string());

        Ok(BranchStatusOutput {
            branch: branch.to_string(),
            base: base.to_string(),
            ahead: ahead as u32,
            behind: behind as u32,
            up_to_date: ahead == 0 && behind == 0,
            common_ancestor,
        })
    }

    /// List commits that exist on `branch` but not on `<remote>/<branch>`.
    pub fn unpushed_commits(&self, branch: &str, remote: &str) -> Result<UnpushedCommitsOutput> {
        let remote_ref = format!("{remote}/{branch}");
        let remote_head = git_cmd(self.session().root(), &["rev-parse", &remote_ref])?;
        let raw = git_cmd(
            self.session().root(),
            &[
                "log",
                "--format=%H%x09%s",
                &format!("{remote_ref}..{branch}"),
            ],
        )?;
        let mut commits = Vec::new();
        for line in raw.lines() {
            if line.is_empty() {
                continue;
            }
            let (sha, summary) = line.split_once('\t').unwrap_or((line, ""));
            let short_sha = sha[..7.min(sha.len())].to_string();
            commits.push(CommitEntry {
                sha: sha.to_string(),
                short_sha,
                summary: summary.to_string(),
            });
        }
        Ok(UnpushedCommitsOutput {
            branch: branch.to_string(),
            remote: remote.to_string(),
            remote_head,
            count: commits.len(),
            commits,
        })
    }

    /// Check whether `commit` is reachable from any remote-tracking ref
    /// under `refs/remotes/<remote>/`.
    pub fn is_pushed(&self, commit: &str, remote: &str) -> Result<IsPushedOutput> {
        let refspace = format!("refs/remotes/{remote}/");
        let raw = git_cmd(
            self.session().root(),
            &[
                "for-each-ref",
                "--contains",
                commit,
                "--format=%(refname)",
                &refspace,
            ],
        )?;
        let refs: Vec<String> = raw
            .lines()
            .filter(|l| !l.is_empty())
            .map(|s| s.to_string())
            .collect();
        let pushed = !refs.is_empty();
        Ok(IsPushedOutput {
            commit: commit.to_string(),
            remote: remote.to_string(),
            pushed,
            refs,
        })
    }

    /// Check whether `tag` exists on `remote` (via `git ls-remote --tags`).
    pub fn tag_pushed(&self, tag: &str, remote: &str) -> Result<TagPushedOutput> {
        let refspec = format!("refs/tags/{tag}");
        let raw = git_cmd(
            self.session().root(),
            &["ls-remote", "--tags", remote, &refspec],
        )?;
        let remote_refs: Vec<String> = raw
            .lines()
            .filter(|l| !l.is_empty())
            .map(|s| s.to_string())
            .collect();
        let pushed = !remote_refs.is_empty();
        Ok(TagPushedOutput {
            tag: tag.to_string(),
            remote: remote.to_string(),
            pushed,
            remote_refs,
        })
    }

    /// Snapshot a worktree's state (branch, tracking, ahead/behind, uncommitted).
    pub fn worktree_state(&self, branch: Option<&str>) -> Result<WorktreeStateOutput> {
        let root = self.session().root();
        let resolved_branch = match branch {
            Some(b) => b.to_string(),
            None => git_cmd(root, &["branch", "--show-current"])?,
        };

        let tracking = resolve_upstream(root)?;

        let (ahead, behind) = if let Some(ref upstream) = tracking {
            let repo = Repository::open(root)?;
            let branch_oid = repo.revparse_single(&resolved_branch)?.id();
            let upstream_oid = repo.revparse_single(upstream)?.id();
            let (a, b) = repo
                .graph_ahead_behind(branch_oid, upstream_oid)
                .unwrap_or((0, 0));
            (a as u32, b as u32)
        } else {
            (0, 0)
        };

        let porcelain = git_cmd(root, &["status", "--porcelain"]).unwrap_or_default();
        let uncommitted = porcelain.lines().filter(|l| !l.is_empty()).count();
        let clean = uncommitted == 0;
        let sync = behind == 0;

        Ok(WorktreeStateOutput {
            branch: resolved_branch,
            tracking,
            ahead,
            behind,
            uncommitted,
            clean,
            sync,
        })
    }
}

/// Resolve the upstream tracking ref of the current HEAD, returning `None`
/// when no upstream is configured.
fn resolve_upstream(root: &Path) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "@{upstream}"])
        .current_dir(root)
        .output()
        .context("failed to spawn git rev-parse @{upstream}")?;
    if output.status.success() {
        let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Ok(if raw.is_empty() { None } else { Some(raw) });
    }
    match output.status.code() {
        Some(128) => Ok(None),
        _ => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            bail!("git rev-parse @{{upstream}}: {stderr}");
        }
    }
}
