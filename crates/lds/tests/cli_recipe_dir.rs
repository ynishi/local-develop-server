//! Integration tests for `lds recipe-dir add|list|remove`.
//!
//! Each test spawns the real `lds` binary (built by Cargo) and overrides
//! `HOME` with a temporary directory so that `~/.config/lds/config.toml`
//! is isolated and does not touch the developer's actual configuration.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Return the path to the compiled `lds` binary.
///
/// Cargo sets `CARGO_BIN_EXE_lds` in integration test processes; we fall back
/// to `<manifest_dir>/../../target/debug/lds` for direct `cargo test` runs.
fn lds_bin() -> String {
    std::env::var("CARGO_BIN_EXE_lds").unwrap_or_else(|_| {
        format!(
            "{}/../../target/debug/lds",
            env!("CARGO_MANIFEST_DIR")
        )
    })
}

/// Spawn `lds <args>` with `HOME` overridden to `home_dir`.
fn run_lds(home_dir: &Path, args: &[&str]) -> Output {
    Command::new(lds_bin())
        .env("HOME", home_dir)
        .args(args)
        .output()
        .expect("failed to spawn lds binary")
}

/// Read the contents of `<home>/.config/lds/config.toml` (or empty string if absent).
fn read_config(home_dir: &Path) -> String {
    let path = home_dir.join(".config/lds/config.toml");
    fs::read_to_string(path).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// TC-1: `add` on a fresh (no config.toml) system creates the file and writes
/// the absolute path.  Tilde literals must NOT appear on disk (crux 2).
#[test]
fn test_add_creates_config_with_absolute_path() {
    let tmp = TempDir::new().expect("TempDir::new");
    let home = tmp.path();

    // Use an absolute target dir (no tilde) so the test is HOME-independent.
    let recipe_dir = tmp.path().join("my-recipes");
    let recipe_dir_str = recipe_dir.to_string_lossy().to_string();

    let out = run_lds(home, &["recipe-dir", "add", &recipe_dir_str]);
    assert!(
        out.status.success(),
        "add should succeed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let toml = read_config(home);
    assert!(
        toml.contains(recipe_dir.to_string_lossy().as_ref()),
        "config.toml should contain the absolute path; got:\n{toml}"
    );
    // Crux 2: tilde literal must not appear on disk.
    assert!(
        !toml.contains('~'),
        "tilde literal must not be stored in config.toml; got:\n{toml}"
    );
}

/// TC-2: `add` with a tilde path expands to an absolute path before writing.
/// We pass `~/cli-test-recipes` and verify the stored path begins with the
/// value of `HOME` (i.e. the tmp dir), not with `~`.
#[test]
fn test_add_expands_tilde_to_absolute() {
    let tmp = TempDir::new().expect("TempDir::new");
    let home = tmp.path();

    let out = run_lds(home, &["recipe-dir", "add", "~/cli-test-recipes"]);
    assert!(
        out.status.success(),
        "add with tilde should succeed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let toml = read_config(home);
    let expected_prefix = home.to_string_lossy().to_string();
    assert!(
        toml.contains(&expected_prefix),
        "stored path should start with HOME ({expected_prefix}); got:\n{toml}"
    );
    assert!(
        !toml.contains('~'),
        "tilde literal must not be stored in config.toml (crux 2); got:\n{toml}"
    );
}

/// TC-3: `add` preserves comments and unrelated sections in an existing
/// config.toml (crux 2 — patch-safe write).
#[test]
fn test_add_preserves_comments_and_other_sections() {
    let tmp = TempDir::new().expect("TempDir::new");
    let home = tmp.path();

    // Seed a config.toml that contains a comment and a [paths] section.
    let config_dir = home.join(".config/lds");
    fs::create_dir_all(&config_dir).expect("create config dir");
    let config_path = config_dir.join("config.toml");
    fs::write(
        &config_path,
        "# My custom lds configuration\n\
         [paths]\n\
         global_justfile = \"/opt/lds/justfile\"\n",
    )
    .expect("write seed config");

    let recipe_dir = tmp.path().join("new-recipes");
    let recipe_dir_str = recipe_dir.to_string_lossy().to_string();

    let out = run_lds(home, &["recipe-dir", "add", &recipe_dir_str]);
    assert!(
        out.status.success(),
        "add should succeed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let toml = read_config(home);

    // Comment must survive.
    assert!(
        toml.contains("# My custom lds configuration"),
        "comment must be preserved; got:\n{toml}"
    );
    // [paths] section must survive.
    assert!(
        toml.contains("[paths]"),
        "[paths] section must be preserved; got:\n{toml}"
    );
    assert!(
        toml.contains("global_justfile"),
        "global_justfile entry must be preserved; got:\n{toml}"
    );
    // The new entry must be present.
    assert!(
        toml.contains(recipe_dir.to_string_lossy().as_ref()),
        "new recipe dir must be added; got:\n{toml}"
    );
}

/// TC-4: `list` prints one path per line in declaration order.
#[test]
fn test_list_prints_paths_in_order() {
    let tmp = TempDir::new().expect("TempDir::new");
    let home = tmp.path();

    let dir_a = tmp.path().join("alpha");
    let dir_b = tmp.path().join("beta");

    run_lds(home, &["recipe-dir", "add", &dir_a.to_string_lossy()]);
    run_lds(home, &["recipe-dir", "add", &dir_b.to_string_lossy()]);

    let out = run_lds(home, &["recipe-dir", "list"]);
    assert!(
        out.status.success(),
        "list should succeed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "expected 2 lines; got:\n{stdout}");

    // Order must match insertion order.
    assert!(
        lines[0].contains(dir_a.to_string_lossy().as_ref()),
        "first line should be dir_a; got:\n{stdout}"
    );
    assert!(
        lines[1].contains(dir_b.to_string_lossy().as_ref()),
        "second line should be dir_b; got:\n{stdout}"
    );
}

/// TC-5: `remove` deletes the matching entry and leaves the rest intact.
#[test]
fn test_remove_deletes_entry() {
    let tmp = TempDir::new().expect("TempDir::new");
    let home = tmp.path();

    let dir_a = tmp.path().join("alpha");
    let dir_b = tmp.path().join("beta");

    run_lds(home, &["recipe-dir", "add", &dir_a.to_string_lossy()]);
    run_lds(home, &["recipe-dir", "add", &dir_b.to_string_lossy()]);

    let out = run_lds(home, &["recipe-dir", "remove", &dir_a.to_string_lossy()]);
    assert!(
        out.status.success(),
        "remove should succeed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let list_out = run_lds(home, &["recipe-dir", "list"]);
    let stdout = String::from_utf8_lossy(&list_out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();

    assert_eq!(lines.len(), 1, "only one entry should remain; got:\n{stdout}");
    assert!(
        lines[0].contains(dir_b.to_string_lossy().as_ref()),
        "remaining entry should be dir_b; got:\n{stdout}"
    );
}

/// TC-6: `remove` of a non-existent path exits with code 1 and writes to stderr.
#[test]
fn test_remove_not_found_exits_one() {
    let tmp = TempDir::new().expect("TempDir::new");
    let home = tmp.path();

    let out = run_lds(
        home,
        &["recipe-dir", "remove", "/no/such/directory"],
    );

    assert!(
        !out.status.success(),
        "remove of non-existent path should exit non-zero"
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "exit code should be 1"
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.is_empty(),
        "stderr should contain an error message; got nothing"
    );
}

/// TC-7: `add` of the same path twice is a no-op on the second call (exit 0,
/// no duplicate in config.toml).
#[test]
fn test_add_duplicate_is_noop() {
    let tmp = TempDir::new().expect("TempDir::new");
    let home = tmp.path();

    let recipe_dir = tmp.path().join("my-recipes");
    let recipe_dir_str = recipe_dir.to_string_lossy().to_string();

    let first = run_lds(home, &["recipe-dir", "add", &recipe_dir_str]);
    assert!(first.status.success(), "first add should succeed");

    let second = run_lds(home, &["recipe-dir", "add", &recipe_dir_str]);
    assert!(
        second.status.success(),
        "second add (duplicate) should exit 0; stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );

    // Warn must go to stderr.
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains("warn") || stderr.contains("already"),
        "second add should emit a warning; stderr: {stderr}"
    );

    // Only one entry in config.toml.
    let list_out = run_lds(home, &["recipe-dir", "list"]);
    let stdout = String::from_utf8_lossy(&list_out.stdout);
    let count = stdout.lines().count();
    assert_eq!(count, 1, "should have exactly 1 entry after duplicate add; got:\n{stdout}");
}
