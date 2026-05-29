use lds_sandbox::fs::SandboxFs;
use lds_sandbox::python::SandboxPython;

fn python_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[test]
fn fs_full_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let fs = SandboxFs::new(tmp.path()).unwrap();

    // Initial write (no snapshot since file is new)
    let r1 = fs.write("doc.txt", "v1: initial\n").unwrap();
    assert!(r1.snapshot_id.is_none());

    // Edit creates snapshot
    let r2 = fs.edit("doc.txt", "v1", "v2", false).unwrap();
    assert!(r2.snapshot_id.is_some());

    // Append creates snapshot
    let r3 = fs.append("doc.txt", "line2\n").unwrap();
    assert!(r3.snapshot_id.is_some());

    // Read shows current state
    let content = fs.read("doc.txt", None, None).unwrap();
    assert!(content.contains("v2"));
    assert!(content.contains("line2"));

    // History has 2 snapshots (edit + append)
    let history = fs.history("doc.txt").unwrap();
    assert_eq!(history.len(), 2);

    // Rollback to most recent (pre-append state)
    fs.rollback("doc.txt", None).unwrap();
    let after_rollback = fs.read("doc.txt", None, None).unwrap();
    assert!(after_rollback.contains("v2"));
    assert!(!after_rollback.contains("line2"));

    // History now has 3 (the rollback itself created a snapshot)
    let history2 = fs.history("doc.txt").unwrap();
    assert_eq!(history2.len(), 3);
}

#[test]
fn fs_rollback_by_specific_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let fs = SandboxFs::new(tmp.path()).unwrap();

    std::fs::write(tmp.path().join("file.txt"), "original").unwrap();
    let r1 = fs.write("file.txt", "step1").unwrap();
    let _r2 = fs.write("file.txt", "step2").unwrap();
    let _r3 = fs.write("file.txt", "step3").unwrap();

    let original_snap = r1.snapshot_id.unwrap();
    fs.rollback("file.txt", Some(&original_snap)).unwrap();
    let content = std::fs::read_to_string(tmp.path().join("file.txt")).unwrap();
    assert_eq!(content, "original");
}

#[tokio::test]
async fn python_sandbox_blocks_dangerous_modules() {
    if !python_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let py = SandboxPython::new(tmp.path()).with_timeout(5);

    for bad_script in &[
        "import subprocess; subprocess.run(['echo'])",
        "import shutil; shutil.rmtree('/tmp/x')",
        "import ctypes",
        "import multiprocessing",
        "import os; os.system('echo bad')",
        "import os; os.fork()",
    ] {
        let result = py.execute(bad_script).await.unwrap();
        assert_ne!(
            result.exit_code, 0,
            "expected failure for script: {bad_script}"
        );
    }
}

#[tokio::test]
async fn python_sandbox_allows_safe_workflows() {
    if !python_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let py = SandboxPython::new(tmp.path()).with_timeout(5);

    let script = r#"
import json
import re
data = {"key": "value", "count": 42}
encoded = json.dumps(data)
match = re.search(r'\d+', encoded)
print(encoded)
print(match.group())
"#;
    let result = py.execute(script).await.unwrap();
    assert_eq!(result.exit_code, 0);
    assert!(result.stdout.contains("\"key\""));
    assert!(result.stdout.contains("42"));
}

#[tokio::test]
async fn python_file_execution_uses_preamble() {
    if !python_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let script_path = tmp.path().join("script.py");
    std::fs::write(&script_path, "import subprocess; print('should not reach')").unwrap();

    let py = SandboxPython::new(tmp.path()).with_timeout(5);
    let result = py.execute_file(&script_path).await.unwrap();
    assert_ne!(result.exit_code, 0);
}

#[tokio::test]
async fn python_fs_combined_workflow() {
    if !python_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let fs = SandboxFs::new(tmp.path()).unwrap();
    let py = SandboxPython::new(tmp.path()).with_timeout(5);

    // Write a data file via SandboxFs
    fs.write("data.json", r#"{"items": [1, 2, 3]}"#).unwrap();

    // Process it via SandboxPython
    let script = r#"
import json
with open('data.json') as f:
    data = json.load(f)
print(sum(data['items']))
"#;
    let result = py.execute(script).await.unwrap();
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout.trim(), "6");
}
