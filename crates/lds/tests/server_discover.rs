//! Regression test for issue #2: the stdio server must not exit when a
//! client sends `server/discover` (the MCP 2026-07-28 pre-init bootstrap
//! probe) before `initialize`.
//!
//! Talks raw newline-delimited JSON-RPC to the built `lds` binary so the
//! exact pre-init message order is under test control.

use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

/// Upper bound for each wire step, so a regression fails instead of hanging.
const STEP_TIMEOUT: Duration = Duration::from_secs(15);

async fn send(stdin: &mut ChildStdin, msg: &Value) {
    let mut line = msg.to_string();
    line.push('\n');
    timeout(STEP_TIMEOUT, stdin.write_all(line.as_bytes()))
        .await
        .expect("timed out writing to lds stdin")
        .expect("write to lds stdin");
    timeout(STEP_TIMEOUT, stdin.flush())
        .await
        .expect("timed out flushing lds stdin")
        .expect("flush lds stdin");
}

/// Read stdout lines until the JSON-RPC response carrying `id` arrives.
async fn read_response(lines: &mut Lines<BufReader<ChildStdout>>, id: i64) -> Value {
    loop {
        let line = timeout(STEP_TIMEOUT, lines.next_line())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for response id {id}"))
            .expect("read lds stdout")
            .unwrap_or_else(|| panic!("lds closed stdout before responding to id {id}"));
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if msg.get("id") == Some(&json!(id)) {
            return msg;
        }
    }
}

#[tokio::test]
async fn server_discover_before_initialize_keeps_connection_open() {
    // Plain temp dir (no `.git` / `justfile`): no session auto-start, and
    // nothing under the real repo is touched.
    let cwd = tempfile::tempdir().expect("tempdir");
    let mut child = Command::new(env!("CARGO_BIN_EXE_lds"))
        .current_dir(cwd.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn lds");
    let mut stdin = child.stdin.take().expect("lds stdin");
    let mut lines = BufReader::new(child.stdout.take().expect("lds stdout")).lines();

    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "server/discover", "params": {}}),
    )
    .await;
    let discover = read_response(&mut lines, 1).await;
    eprintln!("server/discover response: {discover}");
    assert!(
        discover.get("result").is_some() || discover.get("error").is_some(),
        "server/discover must get a JSON-RPC response, got: {discover}"
    );

    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "server-discover-regression", "version": "0.0.0"}
            }
        }),
    )
    .await;
    let init = read_response(&mut lines, 2).await;
    assert!(
        init.get("result").is_some(),
        "initialize after server/discover must succeed, got: {init}"
    );
    assert!(
        init.get("error").is_none(),
        "initialize after server/discover returned an error: {init}"
    );

    let status = child.try_wait().expect("query lds process status");
    assert!(
        status.is_none(),
        "lds exited after server/discover + initialize: {status:?}"
    );
    // `kill_on_drop(true)` terminates the child when `child` goes out of scope.
}
