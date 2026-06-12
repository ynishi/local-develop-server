//! GitHub CLI wrapper module for local-develop-server (lds).
//!
//! Provides [`GhModule`] — a read-only wrapper around the `gh` CLI subprocess.
//! All write operations (`gh pr create`, `gh issue create`, `gh release create`,
//! `gh pr merge`) are intentionally absent: they are structurally excluded from
//! the MCP surface per the cross-cutting AI safety rule (write tool AI-禁止).
//!
//! # Security design
//!
//! Every subprocess call goes through [`gh_cmd`], which:
//! 1. Calls [`gh_auth_check`] first — fails fast if not authenticated.
//! 2. Uses `Command::new("gh").args(args)` (shell=false, arg-by-arg).
//!    No string concatenation or `sh -c` is used anywhere in this crate.

use anyhow::{Context, Result, bail};
use lds_core::Session;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

/// GitHub CLI subprocess wrapper with session context.
///
/// Holds a reference to the active [`Session`] to derive the repository root
/// for subprocess invocations. Only read operations are exposed; write
/// operations (pr create / issue create / release create / pr merge) are
/// structurally absent — they are never implemented in this struct.
pub struct GhModule {
    session: Arc<Session>,
}

impl GhModule {
    /// Creates a new `GhModule` bound to the given session.
    ///
    /// # Arguments
    ///
    /// * `session` — the active session providing repository root context.
    ///
    /// # Returns
    ///
    /// A new `GhModule`. Authentication is NOT checked here; it is checked
    /// per-invocation inside [`gh_cmd`] to guard against token expiry between
    /// session creation and tool invocation.
    pub fn new(session: Arc<Session>) -> Self {
        Self { session }
    }

    /// Returns the `gh auth status` output for the current session root.
    ///
    /// # Returns
    ///
    /// Stdout of `gh auth status` on success.
    ///
    /// # Errors
    ///
    /// Returns an error if `gh` is not installed, not authenticated, or the
    /// subprocess fails. Authentication check is performed before the call.
    pub fn auth_status(&self) -> Result<String> {
        let cwd = self.session.root();
        // auth_status calls gh_auth_check explicitly then reads its output.
        // We invoke gh_auth_check first for the typed error message, then
        // re-run to capture stdout (gh auth status exits 0 when authed).
        gh_auth_check(cwd)?;
        let output = Command::new("gh")
            .args(["auth", "status"])
            .current_dir(cwd)
            .output()
            .context("failed to run gh auth status")?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Lists open pull requests in the current repository.
    ///
    /// # Arguments
    ///
    /// * `limit` — maximum number of PRs to return (passed to `--limit`).
    ///
    /// # Returns
    ///
    /// JSON string with fields: `number`, `title`, `state`, `author`.
    ///
    /// # Errors
    ///
    /// Returns an error if not authenticated, if `gh` is unavailable, or if
    /// the repository has no pull requests and `gh` exits non-zero.
    pub fn pr_list(&self, limit: usize) -> Result<String> {
        let cwd = self.session.root();
        let limit_str = limit.to_string();
        gh_cmd(
            cwd,
            &[
                "pr",
                "list",
                "--json",
                "number,title,state,author",
                "--limit",
                &limit_str,
            ],
        )
    }

    /// Returns details of a single pull request.
    ///
    /// # Arguments
    ///
    /// * `number` — the pull request number.
    ///
    /// # Returns
    ///
    /// JSON string with fields: `number`, `title`, `state`, `author`, `body`,
    /// `baseRefName`, `headRefName`, `url`.
    ///
    /// # Errors
    ///
    /// Returns an error if not authenticated, if the PR does not exist, or if
    /// the subprocess fails.
    pub fn pr_view(&self, number: u64) -> Result<String> {
        let cwd = self.session.root();
        let number_str = number.to_string();
        gh_cmd(
            cwd,
            &[
                "pr",
                "view",
                &number_str,
                "--json",
                "number,title,state,author,body,baseRefName,headRefName,url",
            ],
        )
    }

    /// Returns the unified diff of a pull request.
    ///
    /// # Arguments
    ///
    /// * `number` — the pull request number.
    ///
    /// # Returns
    ///
    /// The raw diff text produced by `gh pr diff`.
    ///
    /// # Errors
    ///
    /// Returns an error if not authenticated, if the PR does not exist, or if
    /// the subprocess fails.
    pub fn pr_diff(&self, number: u64) -> Result<String> {
        let cwd = self.session.root();
        let number_str = number.to_string();
        gh_cmd(cwd, &["pr", "diff", &number_str])
    }

    /// Lists issues in the current repository.
    ///
    /// # Arguments
    ///
    /// * `limit` — maximum number of issues to return (passed to `--limit`).
    ///
    /// # Returns
    ///
    /// JSON string with fields: `number`, `title`, `state`.
    ///
    /// # Errors
    ///
    /// Returns an error if not authenticated, if `gh` is unavailable, or if
    /// the subprocess fails.
    pub fn issue_list(&self, limit: usize) -> Result<String> {
        let cwd = self.session.root();
        let limit_str = limit.to_string();
        gh_cmd(
            cwd,
            &[
                "issue",
                "list",
                "--json",
                "number,title,state",
                "--limit",
                &limit_str,
            ],
        )
    }

    /// Returns details of a single issue.
    ///
    /// # Arguments
    ///
    /// * `number` — the issue number.
    ///
    /// # Returns
    ///
    /// JSON string with fields: `number`, `title`, `state`, `author`, `body`,
    /// `url`.
    ///
    /// # Errors
    ///
    /// Returns an error if not authenticated, if the issue does not exist, or
    /// if the subprocess fails.
    pub fn issue_view(&self, number: u64) -> Result<String> {
        let cwd = self.session.root();
        let number_str = number.to_string();
        gh_cmd(
            cwd,
            &[
                "issue",
                "view",
                &number_str,
                "--json",
                "number,title,state,author,body,url",
            ],
        )
    }

    /// Returns metadata about the current repository.
    ///
    /// # Returns
    ///
    /// JSON string with fields: `name`, `owner`, `defaultBranchRef`.
    ///
    /// # Errors
    ///
    /// Returns an error if not authenticated, if `gh` is unavailable, or if
    /// the subprocess fails.
    pub fn repo_view(&self) -> Result<String> {
        let cwd = self.session.root();
        gh_cmd(
            cwd,
            &["repo", "view", "--json", "name,owner,defaultBranchRef"],
        )
    }

    /// Lists recent workflow runs for the current repository.
    ///
    /// # Arguments
    ///
    /// * `limit` — maximum number of runs to return (passed to `--limit`).
    ///
    /// # Returns
    ///
    /// JSON string with fields: `status`, `conclusion`, `workflowName`.
    ///
    /// # Errors
    ///
    /// Returns an error if not authenticated, if `gh` is unavailable, or if
    /// the subprocess fails.
    pub fn run_list(&self, limit: usize) -> Result<String> {
        let cwd = self.session.root();
        let limit_str = limit.to_string();
        gh_cmd(
            cwd,
            &[
                "run",
                "list",
                "--json",
                "status,conclusion,workflowName",
                "--limit",
                &limit_str,
            ],
        )
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Checks that the current `gh` session is authenticated.
///
/// Runs `gh auth status` directly (does NOT call [`gh_cmd`] to avoid a
/// circular dependency: `gh_cmd` calls `gh_auth_check`).
///
/// # Arguments
///
/// * `cwd` — working directory passed to the subprocess.
///
/// # Returns
///
/// `Ok(())` when `gh auth status` exits 0 (authenticated).
///
/// # Errors
///
/// Returns a typed error message instructing the user to run `gh auth login`
/// when the exit code is non-zero (not authenticated or `gh` unavailable).
fn gh_auth_check(cwd: &Path) -> Result<()> {
    // Direct Command::new — NOT routed through gh_cmd to avoid circular call.
    let output = Command::new("gh")
        .args(["auth", "status"])
        .current_dir(cwd)
        .output()
        .context("failed to run gh auth status")?;
    if !output.status.success() {
        tracing::warn!(
            error = "gh not authenticated",
            "gh auth status failed; user must run `gh auth login`"
        );
        bail!("gh auth status: not authenticated, run `gh auth login`");
    }
    Ok(())
}

/// Runs a `gh` CLI subcommand and returns stdout as a trimmed string.
///
/// # Security contract
///
/// Arguments are passed **arg-by-arg** via `Command::args` (shell=false).
/// No string concatenation, template interpolation, or `sh -c` is used.
/// This structurally prevents shell injection regardless of argument content.
///
/// # Auth contract
///
/// Calls [`gh_auth_check`] before spawning any subprocess. Every call site
/// is guarded, satisfying the Crux "auth fail-fast before any subprocess"
/// constraint without per-caller opt-in.
///
/// # Arguments
///
/// * `cwd`  — working directory for the subprocess.
/// * `args` — argv slice starting with the subcommand (e.g. `["pr", "list", "--json", ...]`).
///
/// # Returns
///
/// Trimmed stdout on success (exit code 0).
///
/// # Errors
///
/// * Authentication failure from [`gh_auth_check`].
/// * `gh` subprocess not found or failed to spawn.
/// * Non-zero exit code — error includes the subcommand name and stderr.
fn gh_cmd(cwd: &Path, args: &[&str]) -> Result<String> {
    // Crux: auth fail-fast before any subprocess call.
    gh_auth_check(cwd)?;

    // Crux: shell=false, arg-by-arg — no string concatenation, no sh -c.
    let output = Command::new("gh")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("failed to run gh {}", args.first().unwrap_or(&"")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(
            error = %stderr.trim(),
            subcommand = args.first().unwrap_or(&""),
            "gh subprocess exited with non-zero status"
        );
        bail!("gh {}: {}", args.first().unwrap_or(&""), stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
