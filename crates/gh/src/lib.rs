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

    /// Returns details of a single workflow run.
    ///
    /// # Arguments
    ///
    /// * `run_id` — the workflow run ID.
    /// * `repo`   — optional `OWNER/REPO` to override the current directory context.
    ///
    /// # Returns
    ///
    /// JSON string with fields: `status`, `conclusion`, `jobs`, `name`,
    /// `createdAt`, `updatedAt`, `htmlUrl`.
    ///
    /// # Errors
    ///
    /// Returns an error if not authenticated, if the run does not exist, or if
    /// the subprocess fails.
    pub fn run_view(&self, run_id: u64, repo: Option<String>) -> Result<String> {
        let cwd = self.session.root();
        let run_id_str = run_id.to_string();
        let mut args: Vec<&str> = vec![
            "run",
            "view",
            &run_id_str,
            "--json",
            "status,conclusion,jobs,name,createdAt,updatedAt,htmlUrl",
        ];
        if let Some(r) = repo.as_ref() {
            args.push("--repo");
            args.push(r.as_str());
        }
        gh_cmd(cwd, &args)
    }

    /// Returns the failed-step log of a workflow run, parsed into structured JSON.
    ///
    /// # Arguments
    ///
    /// * `run_id`     — the workflow run ID.
    /// * `repo`       — optional `OWNER/REPO` to override the current directory context.
    /// * `tail_lines` — number of log lines per failed step to include (default 20).
    ///
    /// # Returns
    ///
    /// JSON string `{ "failed_steps": [{ "job_name", "step_name", "log_tail" }] }`.
    /// On parse failure: `{ "failed_steps": [], "raw_output": "<raw stdout>" }`.
    ///
    /// # Errors
    ///
    /// Returns an error if not authenticated, if the run does not exist, or if
    /// the subprocess fails. Parse errors produce a fallback JSON (no error).
    pub fn run_log_failed(
        &self,
        run_id: u64,
        repo: Option<String>,
        tail_lines: Option<usize>,
    ) -> Result<String> {
        let cwd = self.session.root();
        let run_id_str = run_id.to_string();
        let mut args: Vec<&str> = vec!["run", "view", &run_id_str, "--log-failed"];
        if let Some(r) = repo.as_ref() {
            args.push("--repo");
            args.push(r.as_str());
        }
        let raw = gh_cmd(cwd, &args)?;
        let n = tail_lines.unwrap_or(20);
        Ok(parse_log_failed_text(&raw, n))
    }

    /// Returns the jobs of a workflow run.
    ///
    /// # Arguments
    ///
    /// * `run_id` — the workflow run ID.
    /// * `repo`   — optional `OWNER/REPO` to override the current directory context.
    ///
    /// # Returns
    ///
    /// JSON string with the `jobs` array for the given run.
    ///
    /// # Errors
    ///
    /// Returns an error if not authenticated, if the run does not exist, or if
    /// the subprocess fails.
    pub fn run_jobs(&self, run_id: u64, repo: Option<String>) -> Result<String> {
        let cwd = self.session.root();
        let run_id_str = run_id.to_string();
        let mut args: Vec<&str> = vec![
            "run",
            "view",
            &run_id_str,
            "--json",
            "jobs",
            "--jq",
            ".jobs[]",
        ];
        if let Some(r) = repo.as_ref() {
            args.push("--repo");
            args.push(r.as_str());
        }
        gh_cmd(cwd, &args)
    }

    /// Returns details of a single release by tag.
    ///
    /// # Arguments
    ///
    /// * `tag`  — the release tag (e.g. `"v1.0.0"`).
    /// * `repo` — optional `OWNER/REPO` to override the current directory context.
    ///
    /// # Returns
    ///
    /// JSON string with fields: `name`, `tagName`, `publishedAt`, `assets`,
    /// `body`, `url`.
    ///
    /// # Errors
    ///
    /// Returns an error if not authenticated, if the tag does not exist, or if
    /// the subprocess fails.
    pub fn release_view(&self, tag: String, repo: Option<String>) -> Result<String> {
        let cwd = self.session.root();
        let mut args: Vec<&str> = vec![
            "release",
            "view",
            &tag,
            "--json",
            "name,tagName,publishedAt,assets,body,url",
        ];
        if let Some(r) = repo.as_ref() {
            args.push("--repo");
            args.push(r.as_str());
        }
        gh_cmd(cwd, &args)
    }

    /// Lists releases in the repository.
    ///
    /// # Arguments
    ///
    /// * `limit` — maximum number of releases to return (passed to `--limit`).
    /// * `repo`  — optional `OWNER/REPO` to override the current directory context.
    ///
    /// # Returns
    ///
    /// JSON string array of releases.
    ///
    /// # Errors
    ///
    /// Returns an error if not authenticated, if `gh` is unavailable, or if
    /// the subprocess fails.
    pub fn release_list(&self, limit: usize, repo: Option<String>) -> Result<String> {
        let cwd = self.session.root();
        let limit_str = limit.to_string();
        let mut args: Vec<&str> = vec!["release", "list", "--limit", &limit_str];
        if let Some(r) = repo.as_ref() {
            args.push("--repo");
            args.push(r.as_str());
        }
        gh_cmd(cwd, &args)
    }

    /// Lists workflows in the repository.
    ///
    /// # Arguments
    ///
    /// * `repo` — optional `OWNER/REPO` to override the current directory context.
    ///
    /// # Returns
    ///
    /// JSON string with fields: `name`, `state`, `id`.
    ///
    /// # Errors
    ///
    /// Returns an error if not authenticated, if `gh` is unavailable, or if
    /// the subprocess fails.
    pub fn workflow_list(&self, repo: Option<String>) -> Result<String> {
        let cwd = self.session.root();
        let mut args: Vec<&str> = vec!["workflow", "list", "--json", "name,state,id"];
        if let Some(r) = repo.as_ref() {
            args.push("--repo");
            args.push(r.as_str());
        }
        gh_cmd(cwd, &args)
    }

    /// Returns details of a single workflow by name or ID.
    ///
    /// # Arguments
    ///
    /// * `name_or_id` — the workflow file name, workflow name, or numeric ID.
    /// * `repo`       — optional `OWNER/REPO` to override the current directory context.
    ///
    /// # Returns
    ///
    /// JSON string with fields: `name`, `state`, `path`, `id`.
    ///
    /// # Errors
    ///
    /// Returns an error if not authenticated, if the workflow does not exist, or
    /// if the subprocess fails.
    pub fn workflow_view(&self, name_or_id: String, repo: Option<String>) -> Result<String> {
        let cwd = self.session.root();
        let mut args: Vec<&str> = vec![
            "workflow",
            "view",
            &name_or_id,
            "--json",
            "name,state,path,id",
        ];
        if let Some(r) = repo.as_ref() {
            args.push("--repo");
            args.push(r.as_str());
        }
        gh_cmd(cwd, &args)
    }

    /// Returns the CI check statuses for a pull request.
    ///
    /// # Arguments
    ///
    /// * `number` — the pull request number.
    /// * `repo`   — optional `OWNER/REPO` to override the current directory context.
    ///
    /// # Returns
    ///
    /// JSON string with fields: `name`, `status`, `conclusion`, `link`,
    /// `workflow`.
    ///
    /// # Errors
    ///
    /// Returns an error if not authenticated, if the PR does not exist, or if
    /// the subprocess fails.
    pub fn pr_checks(&self, number: u64, repo: Option<String>) -> Result<String> {
        let cwd = self.session.root();
        let number_str = number.to_string();
        let mut args: Vec<&str> = vec![
            "pr",
            "checks",
            &number_str,
            "--json",
            "name,status,conclusion,link,workflow",
        ];
        if let Some(r) = repo.as_ref() {
            args.push("--repo");
            args.push(r.as_str());
        }
        gh_cmd(cwd, &args)
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

// ---------------------------------------------------------------------------
// Parse helpers
// ---------------------------------------------------------------------------

/// Parses `gh run view --log-failed` TSV output into a structured JSON string.
///
/// Each line of the output is expected to be tab-separated with four columns:
/// `job_name \t step_name \t timestamp \t log_line`.
///
/// Lines that do not conform to this format are silently skipped (defensive
/// parsing — gh CLI version drift is a known risk).
///
/// # Returns
///
/// A JSON string of the form:
/// ```json
/// { "failed_steps": [{ "job_name": "...", "step_name": "...", "log_tail": "..." }] }
/// ```
///
/// If parsing yields no groups (e.g. malformed input with no valid TSV lines),
/// the fallback form is returned:
/// ```json
/// { "failed_steps": [], "raw_output": "<trimmed raw stdout>" }
/// ```
///
/// This function never panics; `.unwrap()` / `.expect()` are absent.
pub(crate) fn parse_log_failed_text(raw: &str, tail_lines: usize) -> String {
    use std::collections::BTreeMap;

    // (job_name, step_name) -> Vec<log_line>
    let mut groups: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();

    for line in raw.lines() {
        // Expected format: job_name \t step_name \t timestamp \t log_line
        let parts: Vec<&str> = line.splitn(4, '\t').collect();
        if parts.len() < 4 {
            continue; // defensive: skip malformed / header / blank lines
        }
        let job_name = parts[0].to_string();
        let step_name = parts[1].to_string();
        // parts[2] = timestamp — intentionally discarded
        let log_line = parts[3].to_string();
        groups
            .entry((job_name, step_name))
            .or_default()
            .push(log_line);
    }

    if groups.is_empty() {
        // Fallback: no parseable TSV lines found — return raw output for diagnosis.
        let v = serde_json::json!({
            "failed_steps": [],
            "raw_output": raw.trim()
        });
        // serde_json::Value::to_string() is infallible for a well-formed Value.
        return v.to_string();
    }

    let failed_steps: Vec<serde_json::Value> = groups
        .into_iter()
        .map(|((job_name, step_name), lines)| {
            let tail: Vec<&String> = if lines.len() > tail_lines {
                lines[lines.len() - tail_lines..].iter().collect()
            } else {
                lines.iter().collect()
            };
            let log_tail = tail
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            serde_json::json!({
                "job_name": job_name,
                "step_name": step_name,
                "log_tail": log_tail
            })
        })
        .collect();

    let v = serde_json::json!({ "failed_steps": failed_steps });
    v.to_string()
}

// ---------------------------------------------------------------------------
// Unit tests (gh CLI independent)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// (a) Single job / single step with 5 log lines — all 5 should appear in
    /// the tail when `tail_lines >= 5`.
    #[test]
    fn parse_log_failed_single_step() {
        let raw = "\
job1\tstep1\t2024-01-01T00:00:00Z\tline A\n\
job1\tstep1\t2024-01-01T00:00:01Z\tline B\n\
job1\tstep1\t2024-01-01T00:00:02Z\tline C\n\
job1\tstep1\t2024-01-01T00:00:03Z\tline D\n\
job1\tstep1\t2024-01-01T00:00:04Z\tline E";

        let result = parse_log_failed_text(raw, 20);
        let v: serde_json::Value = serde_json::from_str(&result).expect("valid JSON");

        let steps = v["failed_steps"].as_array().expect("array");
        assert_eq!(steps.len(), 1, "expected one failed step");

        let s = &steps[0];
        assert_eq!(s["job_name"], "job1");
        assert_eq!(s["step_name"], "step1");
        let tail = s["log_tail"].as_str().expect("string");
        assert!(tail.contains("line A"), "all 5 lines should be in tail");
        assert!(tail.contains("line E"));
    }

    /// (b) Multiple jobs × multiple steps — groups are keyed by (job, step)
    /// and BTreeMap ordering is stable.
    #[test]
    fn parse_log_failed_multiple_jobs_steps() {
        let raw = "\
jobA\tstep1\t2024-01-01T00:00:00Z\tlogA1\n\
jobA\tstep2\t2024-01-01T00:00:01Z\tlogA2\n\
jobB\tstep1\t2024-01-01T00:00:02Z\tlogB1\n\
jobB\tstep1\t2024-01-01T00:00:03Z\tlogB2";

        let result = parse_log_failed_text(raw, 20);
        let v: serde_json::Value = serde_json::from_str(&result).expect("valid JSON");

        let steps = v["failed_steps"].as_array().expect("array");
        assert_eq!(steps.len(), 3, "expected three (job, step) groups");

        // BTreeMap order: (jobA, step1), (jobA, step2), (jobB, step1)
        assert_eq!(steps[0]["job_name"], "jobA");
        assert_eq!(steps[0]["step_name"], "step1");
        assert_eq!(steps[1]["job_name"], "jobA");
        assert_eq!(steps[1]["step_name"], "step2");
        assert_eq!(steps[2]["job_name"], "jobB");
        assert_eq!(steps[2]["step_name"], "step1");
        let tail_b1 = steps[2]["log_tail"].as_str().expect("string");
        assert!(tail_b1.contains("logB1"));
        assert!(tail_b1.contains("logB2"));
    }

    /// (b2) Tail truncation — only the last `tail_lines` entries per group.
    #[test]
    fn parse_log_failed_tail_truncation() {
        // 5 lines for step1, tail_lines=2 → only last 2 should appear
        let raw = "\
job1\tstep1\t2024-01-01T00:00:00Z\tline1\n\
job1\tstep1\t2024-01-01T00:00:01Z\tline2\n\
job1\tstep1\t2024-01-01T00:00:02Z\tline3\n\
job1\tstep1\t2024-01-01T00:00:03Z\tline4\n\
job1\tstep1\t2024-01-01T00:00:04Z\tline5";

        let result = parse_log_failed_text(raw, 2);
        let v: serde_json::Value = serde_json::from_str(&result).expect("valid JSON");

        let steps = v["failed_steps"].as_array().expect("array");
        let tail = steps[0]["log_tail"].as_str().expect("string");
        assert!(!tail.contains("line1"), "line1 should be truncated");
        assert!(!tail.contains("line2"), "line2 should be truncated");
        assert!(!tail.contains("line3"), "line3 should be truncated");
        assert!(tail.contains("line4"), "line4 should be in tail");
        assert!(tail.contains("line5"), "line5 should be in tail");
    }

    /// (c) Malformed / empty input → fallback JSON with `failed_steps: []`
    /// and `raw_output` field present.
    #[test]
    fn parse_log_failed_malformed_fallback() {
        let raw = "this line has no tabs at all\nanother bad line";

        let result = parse_log_failed_text(raw, 20);
        let v: serde_json::Value = serde_json::from_str(&result).expect("valid JSON");

        let steps = v["failed_steps"].as_array().expect("array");
        assert!(
            steps.is_empty(),
            "failed_steps must be empty for malformed input"
        );

        let raw_output = v["raw_output"].as_str().expect("raw_output present");
        assert!(
            raw_output.contains("no tabs"),
            "raw_output should contain original text"
        );
    }

    /// (c2) Empty input → fallback JSON.
    #[test]
    fn parse_log_failed_empty_input() {
        let result = parse_log_failed_text("", 20);
        let v: serde_json::Value = serde_json::from_str(&result).expect("valid JSON");

        let steps = v["failed_steps"].as_array().expect("array");
        assert!(steps.is_empty());
    }
}
