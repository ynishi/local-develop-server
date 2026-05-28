//! End-to-end MCP wire test: spawns the actual `lds` binary as a
//! subprocess, talks to it over stdio via rmcp, and exercises a
//! representative slice of tools to confirm the protocol surface.
//!
//! These tests run cargo's debug binary; `cargo build` is implied.

use std::path::Path;
use std::process::Command as StdCommand;

use rmcp::{model::CallToolRequestParams, transport::TokioChildProcess, ServiceExt};
use serde_json::{json, Value};
use tokio::process::Command;

fn server_bin() -> String {
    std::env::var("CARGO_BIN_EXE_lds").unwrap_or_else(|_| {
        format!("{}/target/debug/lds", env!("CARGO_MANIFEST_DIR"))
    })
}

async fn connect() -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let cmd = Command::new(server_bin());
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

    let result = client
        .peer()
        .call_tool(call_params("git_status", json!({})))
        .await
        .unwrap();

    // A clean tempdir repo produces empty status output — the wire
    // result should be Ok with empty or whitespace-only content.
    let text = extract_text(&result);
    assert!(
        text.trim().is_empty() || text.contains("CURRENT") || text.contains("\n"),
        "unexpected status text: {text:?}"
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
        .call_tool(call_params(
            "sandbox_read",
            json!({ "path": "note.txt" }),
        ))
        .await
        .unwrap();
    let read_text = extract_text(&read_result);
    assert!(read_text.contains("from e2e"));
    assert!(read_text.contains("second line"));

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn calling_tool_without_session_errors() {
    let client = connect().await;
    // No session_start — git_status should report no session.
    let outcome = client
        .peer()
        .call_tool(call_params("git_status", json!({})))
        .await;
    assert!(outcome.is_err(), "expected error, got {outcome:?}");

    client.cancel().await.unwrap();
}
