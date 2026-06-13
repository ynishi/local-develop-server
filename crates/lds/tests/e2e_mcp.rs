//! End-to-end MCP wire test: spawns the actual `lds` binary as a
//! subprocess, talks to it over stdio via rmcp, and exercises a
//! representative slice of tools to confirm the protocol surface.
//!
//! These tests run cargo's debug binary; `cargo build` is implied.

use std::path::Path;
use std::process::Command as StdCommand;

use rmcp::{
    ServiceError, ServiceExt, model::CallToolRequestParams, model::ErrorCode,
    transport::TokioChildProcess,
};
use serde_json::{Value, json};
use tokio::process::Command;

fn server_bin() -> String {
    std::env::var("CARGO_BIN_EXE_lds")
        .unwrap_or_else(|_| format!("{}/target/debug/lds", env!("CARGO_MANIFEST_DIR")))
}

async fn connect() -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let cmd = Command::new(server_bin());
    let transport = TokioChildProcess::new(cmd).expect("failed to spawn lds server");
    ().serve(transport)
        .await
        .expect("failed to initialize MCP client")
}

/// Spawn the lds server with `dir` as its working directory.
///
/// Use this variant when the test needs to control whether auto-start fires.
/// A dir that contains `.git` or `justfile` will trigger auto-start; a plain
/// tempdir will not (crux §3 — must distinguish the two cases).
async fn connect_in(dir: &std::path::Path) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let mut cmd = Command::new(server_bin());
    cmd.current_dir(dir);
    let transport = TokioChildProcess::new(cmd).expect("failed to spawn lds server");
    ().serve(transport)
        .await
        .expect("failed to initialize MCP client")
}

fn call_params(name: &str, args: Value) -> CallToolRequestParams {
    let req = CallToolRequestParams::new(name.to_string());
    match args {
        Value::Object(map) => req.with_arguments(map),
        _ => req,
    }
}

fn extract_text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct TempRepo {
    dir: tempfile::TempDir,
}

impl TempRepo {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        Self::run_git(dir.path(), &["init", "-b", "main"]);
        Self::run_git(dir.path(), &["config", "user.email", "test@test.com"]);
        Self::run_git(dir.path(), &["config", "user.name", "Test"]);
        std::fs::write(dir.path().join("README.md"), "# test\n").unwrap();
        Self::run_git(dir.path(), &["add", "."]);
        Self::run_git(dir.path(), &["commit", "-m", "initial"]);
        std::fs::create_dir_all(dir.path().join(".worktrees")).unwrap();
        Self { dir }
    }

    fn run_git(dir: &Path, args: &[&str]) {
        StdCommand::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git invocation");
    }

    fn path_str(&self) -> &str {
        self.dir.path().to_str().unwrap()
    }
}

#[tokio::test]
async fn list_tools_includes_static_surface() {
    let client = connect().await;
    let tools = client.peer().list_all_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();

    // Spot-check a representative tool from each module so the wire
    // surface stays in lock-step with what the agents call.
    for expected in [
        "session_start",
        "session_info",
        "git_status",
        "git_log",
        "git_diff",
        "git_commit",
        "git_worktree_add",
        "git_worktree_remove",
        "git_worktree_list",
        "git_merge",
        "git_branch_delete",
        "recipe_list",
        "recipe_run",
        "recipe_logs",
        "sandbox_write",
        "sandbox_read",
        "sandbox_edit",
        "sandbox_append",
        "sandbox_head",
        "sandbox_tail",
        "sandbox_rollback",
        "sandbox_history",
        "sandbox_python",
        "sandbox_python_file",
        "gh_run_view",
        "gh_run_log_failed",
    ] {
        assert!(
            names.contains(&expected),
            "tool {expected:?} missing from list_tools; got {names:?}"
        );
    }

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn session_start_returns_id() {
    let repo = TempRepo::new();
    let client = connect().await;

    let result = client
        .peer()
        .call_tool(call_params(
            "session_start",
            json!({ "root": repo.path_str() }),
        ))
        .await
        .unwrap();

    let text = extract_text(&result);
    assert!(text.contains("session started"));
    assert!(text.contains(repo.path_str()));

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn git_status_round_trip() {
    let repo = TempRepo::new();
    let client = connect().await;

    client
        .peer()
        .call_tool(call_params(
            "session_start",
            json!({ "root": repo.path_str() }),
        ))
        .await
        .unwrap();

    // --- clean phase ---
    // A freshly-initialised repo with only a committed README.md.
    // GitModule::status() uses git2::Repository::statuses() which returns
    // no entries for a clean tree, producing "".  The .worktrees/ dir created
    // by TempRepo::new() is untracked and may appear as Status(WT_NEW), so we
    // accept either an empty string or a Status(…) debug-format entry.
    let result_clean = client
        .peer()
        .call_tool(call_params("git_status", json!({})))
        .await
        .unwrap();
    let text_clean = extract_text(&result_clean);
    assert!(
        text_clean.trim().is_empty() || text_clean.contains("Status("),
        "clean repo: expected empty output or Status(...) entries, got: {text_clean:?}"
    );

    // --- dirty phase ---
    // Write an untracked file; git2 will report it as WT_NEW.
    // The output must contain the Status(…) debug-format token and the
    // file-path token "dirty.txt", verifying the structural contract of
    // GitModule::status() on a modified working tree.
    std::fs::write(repo.dir.path().join("dirty.txt"), "content\n").unwrap();
    let result_dirty = client
        .peer()
        .call_tool(call_params("git_status", json!({})))
        .await
        .unwrap();
    let text_dirty = extract_text(&result_dirty);
    assert!(
        text_dirty.contains("Status(") && text_dirty.contains("dirty.txt"),
        "dirty repo: expected Status(...) entry mentioning dirty.txt, got: {text_dirty:?}"
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn sandbox_write_read_round_trip() {
    let repo = TempRepo::new();
    let client = connect().await;

    client
        .peer()
        .call_tool(call_params(
            "session_start",
            json!({ "root": repo.path_str() }),
        ))
        .await
        .unwrap();

    let write_result = client
        .peer()
        .call_tool(call_params(
            "sandbox_write",
            json!({ "path": "note.txt", "content": "from e2e\nsecond line\n" }),
        ))
        .await
        .unwrap();
    let write_text = extract_text(&write_result);
    assert!(write_text.contains("\"path\": \"note.txt\""));
    assert!(write_text.contains("bytes_written"));

    let read_result = client
        .peer()
        .call_tool(call_params("sandbox_read", json!({ "path": "note.txt" })))
        .await
        .unwrap();
    let read_text = extract_text(&read_result);
    assert!(read_text.contains("from e2e"));
    assert!(read_text.contains("second line"));

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn calling_tool_without_session_errors() {
    // Spawn the server in a plain tempdir that has neither `.git` nor
    // `justfile`, so auto-start does NOT fire. Calling git_status without
    // session_start must return an error (crux §3: non-ProjectRoot CWD path).
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let client = connect_in(tmpdir.path()).await;
    let outcome = client
        .peer()
        .call_tool(call_params("git_status", json!({})))
        .await;
    assert!(outcome.is_err(), "expected error, got {outcome:?}");

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn calling_tool_auto_starts_in_project_root() {
    // Spawn the server with a TempRepo (contains `.git`) as CWD.
    // Auto-start should fire and git_status must succeed without an explicit
    // session_start call (crux §3: ProjectRoot CWD path).
    let repo = TempRepo::new();
    let client = connect_in(repo.dir.path()).await;
    let outcome = client
        .peer()
        .call_tool(call_params("git_status", json!({})))
        .await;
    assert!(
        outcome.is_ok(),
        "expected auto-start to succeed, got {outcome:?}"
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn no_session_error_has_internal_error_code() {
    // Regression: all no-session paths must return the unified error code
    // -32603 (McpError::INTERNAL_ERROR). Previously each handler inlined a
    // separate McpError::internal_error("no session", None) call — this test
    // pins the code so any divergence is caught at the E2E boundary.
    //
    // Use a non-ProjectRoot tmpdir so auto-start does NOT fire (crux §3).
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let client = connect_in(tmpdir.path()).await;
    let outcome = client
        .peer()
        .call_tool(call_params("git_status", json!({})))
        .await;

    match outcome {
        Err(ServiceError::McpError(ref err_data)) => {
            assert_eq!(
                err_data.code,
                ErrorCode::INTERNAL_ERROR,
                "no-session error must use code -32603 (INTERNAL_ERROR), got {:?}",
                err_data.code
            );
        }
        Err(other) => panic!("expected McpError(-32603), got ServiceError variant: {other:?}"),
        Ok(_) => panic!("expected error for no-session call, got Ok"),
    }

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn session_start_recovers_after_previous_root_deleted() {
    // Regression guard for K-239: after the session root has been deleted,
    // `session_start` must succeed on a new root even though
    // `try_plugin_call` (via RecipeModule::list_plugins / check_session_root)
    // would have rejected the call on the dead session before this fix.
    //
    // Step 1: create tempdir A with a minimal justfile.
    let dir_a = tempfile::tempdir().expect("tempdir A");
    std::fs::write(dir_a.path().join("justfile"), "default:\n\t@echo ok\n").unwrap();

    // Step 2: use connect() (non-ProjectRoot spawn) so auto-start does NOT
    // fire — this isolates the session_start path from the auto-start gate.
    let client = connect().await;

    // Step 3: session_start with root = A — must succeed.
    let result = client
        .peer()
        .call_tool(call_params(
            "session_start",
            json!({ "root": dir_a.path().to_str().unwrap() }),
        ))
        .await
        .unwrap();
    let text = extract_text(&result);
    assert!(
        text.contains("session started"),
        "step 3: session_start(A) should succeed, got: {text}"
    );

    // Step 4: recipe_list — sanity check that the session is functional.
    let result = client
        .peer()
        .call_tool(call_params("recipe_list", json!({})))
        .await
        .unwrap();
    assert!(
        result.is_error != Some(true),
        "step 4: recipe_list after session_start(A) should succeed, got: {:?}",
        extract_text(&result)
    );

    // Step 5: delete dir A — the session root is now gone.
    std::fs::remove_dir_all(dir_a.path()).unwrap();

    // Step 6: recipe_list must fail with SessionRootGone.
    let outcome = client
        .peer()
        .call_tool(call_params("recipe_list", json!({})))
        .await;
    match outcome {
        Err(ServiceError::McpError(ref err_data)) => {
            assert!(
                err_data
                    .message
                    .contains("session root path no longer exists"),
                "step 6: expected 'session root path no longer exists' in error message, got: {}",
                err_data.message
            );
        }
        Ok(result) => {
            // Some MCP implementations surface errors as Ok(isError:true).
            let text = extract_text(&result);
            assert!(
                result.is_error == Some(true)
                    && text.contains("session root path no longer exists"),
                "step 6: expected SessionRootGone error, got Ok: {text}"
            );
        }
        Err(other) => panic!("step 6: unexpected error variant: {other:?}"),
    }

    // Step 7: create tempdir B with a minimal justfile.
    let dir_b = tempfile::tempdir().expect("tempdir B");
    std::fs::write(dir_b.path().join("justfile"), "default:\n\t@echo ok\n").unwrap();

    // Step 8: session_start with root = B — this is the core assertion of the fix.
    // Before the fix, try_plugin_call would hit check_session_root on the dead
    // session A and reject session_start entirely.
    let result = client
        .peer()
        .call_tool(call_params(
            "session_start",
            json!({ "root": dir_b.path().to_str().unwrap() }),
        ))
        .await
        .unwrap();
    let text = extract_text(&result);
    assert!(
        text.contains("session started"),
        "step 8: session_start(B) must succeed after root A was deleted, got: {text}"
    );

    // Step 9: recipe_list with B — confirm the new session is fully functional.
    let result = client
        .peer()
        .call_tool(call_params("recipe_list", json!({})))
        .await
        .unwrap();
    assert!(
        result.is_error != Some(true),
        "step 9: recipe_list after session_start(B) should succeed, got: {:?}",
        extract_text(&result)
    );

    client.cancel().await.unwrap();
}
