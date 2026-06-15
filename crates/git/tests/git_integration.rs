use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use lds_core::{Session, SessionConfig};
use lds_git::{GitModule, ResetMode};

fn init_temp_repo(dir: &Path) {
    let run = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
    };
    run(&["init", "-b", "main"]);
    run(&["config", "user.email", "test@test.com"]);
    run(&["config", "user.name", "Test"]);
    std::fs::write(dir.join("README.md"), "init\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "-m", "initial"]);
    std::fs::create_dir_all(dir.join(".worktrees")).unwrap();
}

fn make_session(root: &Path) -> Arc<Session> {
    Arc::new(
        Session::new(SessionConfig {
            root: root.to_path_buf(),
            timeout_secs: Some(30),
            ..Default::default()
        })
        .unwrap(),
    )
}

#[test]
fn worktree_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let mut git = GitModule::new(session);

    // worktree_list: only main worktree, not owned by us yet.
    let list = git.worktree_list().unwrap();
    assert_eq!(list.worktrees.len(), 1);
    assert!(!list.worktrees[0].owned);

    // worktree_add
    let add_result = git
        .worktree_add("test-wt", "feat/test", Some("main"))
        .unwrap();
    assert!(add_result.path.ends_with("test-wt"));
    assert_eq!(add_result.branch, "feat/test");

    // worktree_list: the new worktree is now owned.
    let list = git.worktree_list().unwrap();
    assert!(
        list.worktrees.iter().any(|w| w.owned),
        "expected at least one owned worktree, got: {list:?}"
    );

    // commit in worktree
    let wt_path = tmp.path().join(".worktrees/test-wt");
    std::fs::write(wt_path.join("new_file.txt"), "content\n").unwrap();
    let commit_result = git.commit(&wt_path, "test commit", None).unwrap();
    assert_eq!(commit_result.sha.len(), 40, "expected full SHA-1");
    assert_eq!(commit_result.message, "test commit");
    assert_eq!(commit_result.files_changed, 1);

    // merge back to main
    let merge_result = git.merge("feat/test", "main", tmp.path()).unwrap();
    assert_eq!(merge_result.branch, "feat/test");
    assert_eq!(merge_result.into_branch, "main");
    assert_eq!(merge_result.sha.len(), 40);

    // worktree_remove
    let remove_result = git.worktree_remove("test-wt").unwrap();
    assert!(remove_result.path.ends_with("test-wt"));

    // branch_delete
    let delete_result = git.branch_delete("feat/test").unwrap();
    assert_eq!(delete_result.branch, "feat/test");

    // verify merge landed: new_file.txt should exist in main
    assert!(tmp.path().join("new_file.txt").exists());

    // git log should show the merge commit
    let log = git.log(5).unwrap();
    assert!(
        log.commits
            .iter()
            .any(|c| c.summary.contains("Merge branch")),
        "expected a 'Merge branch' commit in log, got: {log:?}"
    );
}

#[test]
fn ownership_guard_rejects_unowned_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    // create a worktree outside of GitModule (simulating another session)
    Command::new("git")
        .args([
            "worktree",
            "add",
            "-b",
            "other/branch",
            tmp.path().join(".worktrees/foreign").to_str().unwrap(),
            "main",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let foreign_path = tmp.path().join(".worktrees/foreign");

    // commit to unowned worktree should fail
    std::fs::write(foreign_path.join("file.txt"), "x").unwrap();
    let err = git.commit(&foreign_path, "bad commit", None);
    assert!(err.is_err());
    assert!(
        err.unwrap_err()
            .to_string()
            .contains("not owned by this session")
    );

    // branch_delete on unowned branch should fail
    let err = git.branch_delete("other/branch");
    assert!(err.is_err());
    assert!(
        err.unwrap_err()
            .to_string()
            .contains("not owned by this session")
    );
}

#[test]
fn commit_allowed_at_session_root() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    std::fs::write(tmp.path().join("root_file.txt"), "content\n").unwrap();
    let result = git.commit(
        tmp.path(),
        "root commit",
        Some(&["root_file.txt".to_string()]),
    );
    let commit = result.expect("commit at session root");
    assert_eq!(commit.sha.len(), 40);
    assert_eq!(commit.message, "root commit");
}

#[test]
fn status_partitions_staged_unstaged_untracked() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    // Clean state right after the initial commit.
    // The `.worktrees/` directory created by init_temp_repo is itself
    // untracked, so the clean predicate examines staged + unstaged only.
    let status = git.status().unwrap();
    assert!(status.staged.is_empty(), "staged was {:?}", status.staged);
    assert!(
        status.unstaged.is_empty(),
        "unstaged was {:?}",
        status.unstaged
    );
    assert_eq!(status.branch.as_deref(), Some("main"));
    assert!(status.head_sha.is_some());

    // Add an untracked file.
    std::fs::write(tmp.path().join("untracked.txt"), "u\n").unwrap();
    let status = git.status().unwrap();
    assert!(
        status.untracked.iter().any(|p| p.ends_with("untracked.txt")),
        "untracked was {:?}",
        status.untracked
    );

    // Stage it -> staged bucket only.
    Command::new("git")
        .args(["add", "untracked.txt"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let status = git.status().unwrap();
    assert!(
        status.staged.iter().any(|e| e.path.ends_with("untracked.txt")),
        "staged was {:?}",
        status.staged
    );
}

#[test]
fn diff_distinguishes_staged_from_unstaged() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    // Modify README and stage it: that change shows in the staged diff only.
    std::fs::write(tmp.path().join("README.md"), "init\nchanged\n").unwrap();
    Command::new("git")
        .args(["add", "README.md"])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let unstaged = git.diff(false).unwrap();
    assert!(!unstaged.staged);
    assert_eq!(
        unstaged.file_count, 0,
        "expected no unstaged changes, patch was: {:?}",
        unstaged.patch
    );

    let staged = git.diff(true).unwrap();
    assert!(staged.staged);
    assert_eq!(staged.file_count, 1);
    assert!(
        staged.patch.contains("changed"),
        "expected '+changed' line in staged patch, got: {:?}",
        staged.patch
    );

    // Re-modify README without staging: that further change shows in the
    // unstaged diff (worktree-vs-index).
    std::fs::write(tmp.path().join("README.md"), "init\nchanged\nagain\n").unwrap();
    let unstaged = git.diff(false).unwrap();
    assert!(!unstaged.staged);
    assert_eq!(unstaged.file_count, 1);
    assert!(
        unstaged.patch.contains("again"),
        "expected '+again' line in unstaged patch, got: {:?}",
        unstaged.patch
    );
}

#[test]
fn reset_moves_head_back() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    // Capture the pre-reset sha.
    let before = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let before_sha = String::from_utf8_lossy(&before.stdout).trim().to_string();

    // Add a second commit on top.
    std::fs::write(tmp.path().join("two.txt"), "two\n").unwrap();
    Command::new("git")
        .args(["add", "two.txt"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "second"])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    // Reset back to the first commit.
    let result = git
        .reset(tmp.path(), ResetMode::Hard, &before_sha)
        .expect("reset");
    assert!(matches!(result.mode, ResetMode::Hard));
    assert_eq!(result.target, before_sha);
    assert_eq!(result.current_head, before_sha);
    assert_ne!(result.previous_head, result.current_head);
}

#[test]
fn session_release_adopts_orphan_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let mut git = GitModule::new(session);

    // Simulate a worktree left over by a previous session: not owned by us.
    Command::new("git")
        .args([
            "worktree",
            "add",
            "-b",
            "left/over",
            tmp.path().join(".worktrees/leftover").to_str().unwrap(),
            "main",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let leftover = tmp.path().join(".worktrees/leftover");

    // branch_delete must refuse before we adopt.
    assert!(git.branch_delete("left/over").is_err());

    // Adopt: session_release should pick up `leftover` + branch `left/over`.
    let release = git.session_release().expect("session_release");
    // macOS resolves /var/... to /private/var/..., so compare canonical paths.
    let canonical_leftover = leftover.canonicalize().unwrap_or(leftover.clone());
    assert!(
        release.adopted_worktrees.iter().any(|p| {
            let canon = p.canonicalize().unwrap_or_else(|_| p.clone());
            canon == canonical_leftover
        }),
        "expected leftover to be adopted (canonical: {canonical_leftover:?}), got: {release:?}"
    );
    assert!(
        release.adopted_branches.iter().any(|b| b == "left/over"),
        "expected left/over branch adopted, got: {release:?}"
    );

    // After adoption, branch_delete on `left/over` should succeed once the
    // worktree has been removed (a branch can't be deleted while checked out).
    git.worktree_remove("leftover").expect("worktree_remove after adoption");
    git.branch_delete("left/over")
        .expect("branch_delete after adoption");
}
