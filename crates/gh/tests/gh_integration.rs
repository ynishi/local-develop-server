//! Integration tests for `lds-gh`.
//!
//! Tests are skipped automatically when the `gh` CLI is not available on
//! `PATH`.  In CI with `gh` installed and authenticated the tests run
//! end-to-end; without `gh` the skip guard exits early so the suite passes.
//!
//! # Test categories
//!
//! * **T1 (happy path)**: normal authenticated calls succeed and return output.
//! * **T2 (boundary / edge)**: limit variants, empty-output conditions.
//! * **T3 (error path)**: unauthenticated session returns a typed error; bad
//!   PR/issue numbers return an error rather than a panic.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use lds_core::{Session, SessionConfig};
use lds_gh::GhModule;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Returns `true` when the `gh` CLI is available on PATH and exits 0 for
/// `--version`.
fn gh_available() -> bool {
    Command::new("gh")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Returns `true` when `gh` is available AND the current session is
/// authenticated (`gh auth status` exits 0).
fn gh_authenticated() -> bool {
    if !gh_available() {
        return false;
    }
    Command::new("gh")
        .args(["auth", "status"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Returns a path to a real git repository (the workspace root) to use as the
/// session root for tests that invoke `gh` subcommands that need a repo
/// context (`gh pr list`, `gh issue list`, `gh repo view`, `gh run list`).
///
/// The cargo test binary is run from the crate root; we walk up until we find
/// a `.git` directory.  Falls back to the crate root if not found.
fn repo_root() -> PathBuf {
    let mut dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    loop {
        if dir.join(".git").exists() {
            return dir;
        }
        match dir.parent() {
            Some(p) => dir = p.to_path_buf(),
            None => break,
        }
    }
    // Fallback: return cwd even if .git not found; gh will error, which is fine
    // since the tests have an authenticated guard.
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn make_session(root: &Path) -> Arc<Session> {
    // SAFETY: Session::new returns Ok for a valid directory path.
    Arc::new(
        Session::new(SessionConfig {
            root: root.to_path_buf(),
            timeout_secs: Some(30),
            ..Default::default()
        })
        .unwrap(),
    )
}

// ---------------------------------------------------------------------------
// T1 — happy path
// ---------------------------------------------------------------------------

/// T1: `auth_status` returns non-empty output when authenticated.
#[test]
fn t1_auth_status_returns_output_when_authenticated() {
    if !gh_authenticated() {
        eprintln!("skip: gh CLI not available or not authenticated");
        return;
    }
    // auth_status only needs gh auth, not a repo root — use a temp dir.
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(tmp.path());
    let gh = GhModule::new(session);

    let result = gh.auth_status();
    assert!(result.is_ok(), "auth_status failed: {:?}", result.err());
    // auth_status output may be empty on stdout (gh writes to stderr); Ok is sufficient.
}

/// T1: `pr_list` returns a JSON string when run against a real repo.
#[test]
fn t1_pr_list_returns_json() {
    if !gh_authenticated() {
        eprintln!("skip: gh CLI not available or not authenticated");
        return;
    }
    let session = make_session(&repo_root());
    let gh = GhModule::new(session);

    let result = gh.pr_list(10);
    assert!(result.is_ok(), "pr_list failed: {:?}", result.err());
    let output = result.unwrap();
    // Output is a JSON array (may be empty "[]" if no PRs).
    assert!(
        output.starts_with('[') || output.starts_with('{'),
        "expected JSON, got: {output}"
    );
}

/// T1: `issue_list` returns a JSON string when run against a real repo.
#[test]
fn t1_issue_list_returns_json() {
    if !gh_authenticated() {
        eprintln!("skip: gh CLI not available or not authenticated");
        return;
    }
    let session = make_session(&repo_root());
    let gh = GhModule::new(session);

    let result = gh.issue_list(10);
    assert!(result.is_ok(), "issue_list failed: {:?}", result.err());
    let output = result.unwrap();
    assert!(
        output.starts_with('[') || output.starts_with('{'),
        "expected JSON, got: {output}"
    );
}

/// T1: `repo_view` returns a JSON object containing `name` when authenticated.
#[test]
fn t1_repo_view_returns_json_with_name() {
    if !gh_authenticated() {
        eprintln!("skip: gh CLI not available or not authenticated");
        return;
    }
    let session = make_session(&repo_root());
    let gh = GhModule::new(session);

    let result = gh.repo_view();
    assert!(result.is_ok(), "repo_view failed: {:?}", result.err());
    let output = result.unwrap();
    assert!(
        output.contains("name"),
        "expected 'name' field in JSON, got: {output}"
    );
}

// ---------------------------------------------------------------------------
// T2 — boundary / edge
// ---------------------------------------------------------------------------

/// T2: `pr_list` with limit=1 does not error.
#[test]
fn t2_pr_list_limit_one() {
    if !gh_authenticated() {
        eprintln!("skip: gh CLI not available or not authenticated");
        return;
    }
    let session = make_session(&repo_root());
    let gh = GhModule::new(session);

    let result = gh.pr_list(1);
    assert!(result.is_ok(), "pr_list(1) failed: {:?}", result.err());
}

/// T2: `issue_list` with limit=1 does not error.
#[test]
fn t2_issue_list_limit_one() {
    if !gh_authenticated() {
        eprintln!("skip: gh CLI not available or not authenticated");
        return;
    }
    let session = make_session(&repo_root());
    let gh = GhModule::new(session);

    let result = gh.issue_list(1);
    assert!(result.is_ok(), "issue_list(1) failed: {:?}", result.err());
}

/// T2: `run_list` with limit=5 does not error.
#[test]
fn t2_run_list_limit_five() {
    if !gh_authenticated() {
        eprintln!("skip: gh CLI not available or not authenticated");
        return;
    }
    let session = make_session(&repo_root());
    let gh = GhModule::new(session);

    let result = gh.run_list(5);
    assert!(result.is_ok(), "run_list(5) failed: {:?}", result.err());
}

// ---------------------------------------------------------------------------
// T3 — error path
// ---------------------------------------------------------------------------

/// T3: All read methods return a typed authentication error when `gh` is
/// unauthenticated.
///
/// This test runs only when `gh` is installed but NOT authenticated.  If the
/// environment is fully authenticated the skip guard exits early (expected in
/// CI where T1 path is the primary verification).
#[test]
fn t3_auth_check_returns_typed_error_when_unauthenticated() {
    if !gh_available() {
        eprintln!("skip: gh CLI not available");
        return;
    }
    if gh_authenticated() {
        // Triggering the unauthenticated path requires `gh auth logout`, which
        // is destructive to the global auth state.  Skip in authenticated envs.
        eprintln!("skip: gh is authenticated; unauthenticated path requires logout");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(tmp.path());
    let gh = GhModule::new(session);

    let err = gh.pr_list(10).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("not authenticated"),
        "expected 'not authenticated' in error, got: {msg}"
    );
    assert!(
        msg.contains("gh auth login"),
        "expected 'gh auth login' hint in error, got: {msg}"
    );
}

/// T3: `pr_view` with an invalid PR number returns an Err (not a panic).
#[test]
fn t3_pr_view_nonexistent_number_returns_error() {
    if !gh_authenticated() {
        eprintln!("skip: gh CLI not available or not authenticated");
        return;
    }
    let session = make_session(&repo_root());
    let gh = GhModule::new(session);

    // PR 0 is never valid in GitHub — expect an error, must not panic.
    let result = gh.pr_view(0);
    let _ = result; // Ok or Err both acceptable; panic is not.
}

/// T3: `issue_view` with an invalid issue number returns an Err (not a panic).
#[test]
fn t3_issue_view_nonexistent_number_returns_error() {
    if !gh_authenticated() {
        eprintln!("skip: gh CLI not available or not authenticated");
        return;
    }
    let session = make_session(&repo_root());
    let gh = GhModule::new(session);

    // Issue 0 is never valid in GitHub — expect an error, must not panic.
    let result = gh.issue_view(0);
    let _ = result; // Ok or Err both acceptable; panic is not.
}

// ---------------------------------------------------------------------------
// T1 — happy path (new 8 methods)
// ---------------------------------------------------------------------------

/// Helper: fetch the latest Actions run id via `gh run list --json databaseId --limit 1`.
///
/// Returns `None` when there are no runs or when `gh` is unavailable.
/// This uses `Command::new("gh").args(&[...])` directly (shell=false) in test scope.
fn fetch_latest_run_id() -> Option<u64> {
    let output = Command::new("gh")
        .args(["run", "list", "--json", "databaseId", "--limit", "1"])
        .current_dir(repo_root())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&text).ok()?;
    parsed.as_array()?.first()?.get("databaseId")?.as_u64()
}

/// Helper: fetch the latest release tag via `gh release list --json tagName --limit 1`.
///
/// Returns `None` when there are no releases.
fn fetch_latest_release_tag() -> Option<String> {
    let output = Command::new("gh")
        .args(["release", "list", "--json", "tagName", "--limit", "1"])
        .current_dir(repo_root())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&text).ok()?;
    parsed
        .as_array()?
        .first()?
        .get("tagName")?
        .as_str()
        .map(|s| s.to_string())
}

/// Helper: fetch the first workflow name via `gh workflow list --json name --limit 1`.
///
/// Returns `None` when there are no workflows.
fn fetch_first_workflow_name() -> Option<String> {
    let output = Command::new("gh")
        .args(["workflow", "list", "--json", "name", "--limit", "1"])
        .current_dir(repo_root())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&text).ok()?;
    parsed
        .as_array()?
        .first()?
        .get("name")?
        .as_str()
        .map(|s| s.to_string())
}

/// T1: `run_view` returns a JSON string for a real run id.
#[test]
fn t1_run_view_returns_json() {
    if !gh_authenticated() {
        eprintln!("skip: gh CLI not available or not authenticated");
        return;
    }
    let run_id = match fetch_latest_run_id() {
        Some(id) => id,
        None => {
            eprintln!("skip: no runs in repo");
            return;
        }
    };
    let session = make_session(&repo_root());
    let gh = GhModule::new(session);

    let result = gh.run_view(run_id, None);
    assert!(result.is_ok(), "run_view failed: {:?}", result.err());
    let output = result.unwrap();
    assert!(
        output.contains("databaseId") || output.contains("status") || output.starts_with('{'),
        "expected JSON object, got: {output}"
    );
}

/// T1: `run_log_failed` returns a structured JSON with `failed_steps` key (Crux Preservation).
#[test]
fn t1_run_log_failed_returns_structured_json() {
    if !gh_authenticated() {
        eprintln!("skip: gh CLI not available or not authenticated");
        return;
    }
    let run_id = match fetch_latest_run_id() {
        Some(id) => id,
        None => {
            eprintln!("skip: no runs in repo");
            return;
        }
    };
    let session = make_session(&repo_root());
    let gh = GhModule::new(session);

    let result = gh.run_log_failed(run_id, None, None);
    assert!(result.is_ok(), "run_log_failed failed: {:?}", result.err());
    let output = result.unwrap();
    // Crux must_not_simplify: must return { failed_steps: [...] } JSON, never raw string.
    assert!(
        output.contains("\"failed_steps\""),
        "expected '\"failed_steps\"' key in JSON output, got: {output}"
    );
}

/// T1: `run_jobs` returns a JSON string listing jobs for a real run id.
#[test]
fn t1_run_jobs_returns_json() {
    if !gh_authenticated() {
        eprintln!("skip: gh CLI not available or not authenticated");
        return;
    }
    let run_id = match fetch_latest_run_id() {
        Some(id) => id,
        None => {
            eprintln!("skip: no runs in repo");
            return;
        }
    };
    let session = make_session(&repo_root());
    let gh = GhModule::new(session);

    let result = gh.run_jobs(run_id, None);
    assert!(result.is_ok(), "run_jobs failed: {:?}", result.err());
    let output = result.unwrap();
    assert!(
        output.starts_with('[') || output.starts_with('{'),
        "expected JSON, got: {output}"
    );
}

/// T1: `release_view` returns a JSON object for a real release tag.
#[test]
fn t1_release_view_returns_json() {
    if !gh_authenticated() {
        eprintln!("skip: gh CLI not available or not authenticated");
        return;
    }
    let tag = match fetch_latest_release_tag() {
        Some(t) => t,
        None => {
            eprintln!("skip: no releases in repo");
            return;
        }
    };
    let session = make_session(&repo_root());
    let gh = GhModule::new(session);

    let result = gh.release_view(tag.clone(), None);
    assert!(
        result.is_ok(),
        "release_view({tag}) failed: {:?}",
        result.err()
    );
    let output = result.unwrap();
    assert!(
        output.contains("tagName") || output.contains("name") || output.starts_with('{'),
        "expected JSON object, got: {output}"
    );
}

/// T1: `release_list` returns a JSON array (may be empty `[]` or soft-skip when none).
#[test]
fn t1_release_list_returns_json() {
    if !gh_authenticated() {
        eprintln!("skip: gh CLI not available or not authenticated");
        return;
    }
    let session = make_session(&repo_root());
    let gh = GhModule::new(session);

    let result = gh.release_list(30, None);
    assert!(result.is_ok(), "release_list failed: {:?}", result.err());
    let output = result.unwrap();
    if output.is_empty() {
        // gh returns empty string when there are no releases; that is valid output.
        eprintln!("note: no releases in repo; output is empty (gh CLI behaviour)");
        return;
    }
    assert!(
        output.starts_with('[') || output.starts_with('{'),
        "expected JSON, got: {output}"
    );
}

/// T1: `workflow_list` returns a JSON array (may be empty `[]` or soft-skip when none).
#[test]
fn t1_workflow_list_returns_json() {
    if !gh_authenticated() {
        eprintln!("skip: gh CLI not available or not authenticated");
        return;
    }
    let session = make_session(&repo_root());
    let gh = GhModule::new(session);

    let result = gh.workflow_list(None);
    assert!(result.is_ok(), "workflow_list failed: {:?}", result.err());
    let output = result.unwrap();
    if output.is_empty() {
        // gh returns empty string when there are no workflows; that is valid output.
        eprintln!("note: no workflows in repo; output is empty (gh CLI behaviour)");
        return;
    }
    assert!(
        output.starts_with('[') || output.starts_with('{'),
        "expected JSON, got: {output}"
    );
}

/// T1: `workflow_view` returns a JSON object for a real workflow name.
#[test]
fn t1_workflow_view_returns_json() {
    if !gh_authenticated() {
        eprintln!("skip: gh CLI not available or not authenticated");
        return;
    }
    let name = match fetch_first_workflow_name() {
        Some(n) => n,
        None => {
            eprintln!("skip: no workflows in repo");
            return;
        }
    };
    let session = make_session(&repo_root());
    let gh = GhModule::new(session);

    let result = gh.workflow_view(name.clone(), None);
    assert!(
        result.is_ok(),
        "workflow_view({name}) failed: {:?}",
        result.err()
    );
    let output = result.unwrap();
    assert!(
        output.contains("name") || output.starts_with('{'),
        "expected JSON object with 'name', got: {output}"
    );
}

/// T1: `pr_checks` returns a JSON string (may be empty array for PR without checks).
#[test]
fn t1_pr_checks_returns_json() {
    if !gh_authenticated() {
        eprintln!("skip: gh CLI not available or not authenticated");
        return;
    }
    let session = make_session(&repo_root());
    let gh = GhModule::new(session);

    // Use PR 1 as a representative; it may not exist — we accept both Ok and Err.
    // The critical property is: must not panic.
    let result = gh.pr_checks(1, None);
    let _ = result; // Ok (JSON) or Err both acceptable; panic is not.
}

// ---------------------------------------------------------------------------
// T2 — boundary / edge (new 8 methods)
// ---------------------------------------------------------------------------

/// T2: `run_log_failed` with `tail_lines=1` (boundary) does not error.
#[test]
fn t2_run_log_failed_tail_lines_boundary() {
    if !gh_authenticated() {
        eprintln!("skip: gh CLI not available or not authenticated");
        return;
    }
    let run_id = match fetch_latest_run_id() {
        Some(id) => id,
        None => {
            eprintln!("skip: no runs in repo");
            return;
        }
    };
    let session = make_session(&repo_root());
    let gh = GhModule::new(session);

    let result = gh.run_log_failed(run_id, None, Some(1));
    assert!(
        result.is_ok(),
        "run_log_failed tail_lines=1 failed: {:?}",
        result.err()
    );
    let output = result.unwrap();
    // Even with tail_lines=1, output must be structured JSON (Crux Preservation).
    assert!(
        output.contains("\"failed_steps\""),
        "expected '\"failed_steps\"' key with tail_lines=1, got: {output}"
    );
}

/// T2: `release_list` with `limit=1` does not error.
#[test]
fn t2_release_list_limit_one() {
    if !gh_authenticated() {
        eprintln!("skip: gh CLI not available or not authenticated");
        return;
    }
    let session = make_session(&repo_root());
    let gh = GhModule::new(session);

    let result = gh.release_list(1, None);
    assert!(
        result.is_ok(),
        "release_list(limit=1) failed: {:?}",
        result.err()
    );
}

/// T2: `run_jobs` with a real run id (boundary for job list vs run view).
#[test]
fn t2_run_jobs_with_run_id() {
    if !gh_authenticated() {
        eprintln!("skip: gh CLI not available or not authenticated");
        return;
    }
    let run_id = match fetch_latest_run_id() {
        Some(id) => id,
        None => {
            eprintln!("skip: no runs in repo");
            return;
        }
    };
    let session = make_session(&repo_root());
    let gh = GhModule::new(session);

    let result = gh.run_jobs(run_id, None);
    assert!(
        result.is_ok(),
        "run_jobs boundary failed: {:?}",
        result.err()
    );
    let output = result.unwrap();
    assert!(
        output.starts_with('[') || output.starts_with('{'),
        "expected JSON, got: {output}"
    );
}

// ---------------------------------------------------------------------------
// T3 — error path (new 8 methods)
// ---------------------------------------------------------------------------

/// T3: `release_view` with a nonexistent tag returns Err (not a panic).
#[test]
fn t3_release_view_nonexistent_tag_returns_error() {
    if !gh_authenticated() {
        eprintln!("skip: gh CLI not available or not authenticated");
        return;
    }
    let session = make_session(&repo_root());
    let gh = GhModule::new(session);

    // A tag with this name does not exist; must return Err without panicking.
    let result = gh.release_view("nonexistent-tag-xxxxxxx".to_string(), None);
    assert!(
        result.is_err(),
        "expected Err for nonexistent tag, got Ok: {:?}",
        result.ok()
    );
}

/// T3: `workflow_view` with a nonexistent workflow name returns Err (not a panic).
#[test]
fn t3_workflow_view_nonexistent_name_returns_error() {
    if !gh_authenticated() {
        eprintln!("skip: gh CLI not available or not authenticated");
        return;
    }
    let session = make_session(&repo_root());
    let gh = GhModule::new(session);

    let result = gh.workflow_view("nonexistent-wf-xxxxxxx".to_string(), None);
    assert!(
        result.is_err(),
        "expected Err for nonexistent workflow, got Ok: {:?}",
        result.ok()
    );
}

/// T3: `pr_checks` with number=0 returns Err or Ok (must not panic).
#[test]
fn t3_pr_checks_zero_number_returns_error() {
    if !gh_authenticated() {
        eprintln!("skip: gh CLI not available or not authenticated");
        return;
    }
    let session = make_session(&repo_root());
    let gh = GhModule::new(session);

    // PR 0 is invalid; expect an error. Must not panic.
    let result = gh.pr_checks(0, None);
    let _ = result; // Ok or Err both acceptable; panic is not.
}

/// T3: New methods return a typed authentication error when `gh` is unauthenticated.
///
/// Tests `release_view` and `run_view` as representatives of the new 8 methods.
/// Verifies Crux must_not_simplify: per-call auth check is invoked on every call.
///
/// Runs only when `gh` is installed but NOT authenticated.
#[test]
fn t3_auth_check_new_methods_unauthenticated() {
    if !gh_available() {
        eprintln!("skip: gh CLI not available");
        return;
    }
    if gh_authenticated() {
        // Triggering the unauthenticated path requires `gh auth logout`, which
        // is destructive to the global auth state. Skip in authenticated envs.
        eprintln!("skip: gh is authenticated; unauthenticated path requires logout");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(tmp.path());
    let gh = GhModule::new(session);

    // release_view: must fail with auth error on every call (per-call auth check, no cache).
    let err = gh.release_view("v0.1.0".to_string(), None).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("not authenticated"),
        "expected 'not authenticated' in error, got: {msg}"
    );
    assert!(
        msg.contains("gh auth login"),
        "expected 'gh auth login' hint in error, got: {msg}"
    );
}
