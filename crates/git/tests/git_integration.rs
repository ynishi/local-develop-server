use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use lds_core::{Session, SessionConfig};
use lds_git::GitModule;

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
            max_output: None,
            global_recipe_dir: None,
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

    // worktree_list: only main worktree, not owned
    let list = git.worktree_list().unwrap();
    assert!(list.contains("owned: false"));

    // worktree_add
    let result = git.worktree_add("test-wt", "feat/test", Some("main")).unwrap();
    assert!(result.contains("worktree created"));
    assert!(result.contains("feat/test"));

    // worktree_list: new worktree is owned
    let list = git.worktree_list().unwrap();
    assert!(list.contains("owned: true"));

    // commit in worktree
    let wt_path = tmp.path().join(".worktrees/test-wt");
    std::fs::write(wt_path.join("new_file.txt"), "content\n").unwrap();
    let commit_result = git.commit(&wt_path, "test commit", None).unwrap();
    assert!(commit_result.contains("committed"));
    assert!(commit_result.contains("files_changed=1"));

    // merge back to main
    let merge_result = git.merge("feat/test", "main", tmp.path()).unwrap();
    assert!(merge_result.contains("merged"));

    // worktree_remove
    let remove_result = git.worktree_remove("test-wt").unwrap();
    assert!(remove_result.contains("worktree removed"));

    // branch_delete
    let delete_result = git.branch_delete("feat/test").unwrap();
    assert!(delete_result.contains("branch deleted"));

    // verify merge landed: new_file.txt should exist in main
    assert!(tmp.path().join("new_file.txt").exists());

    // git log should show the merge commit
    let log = git.log(5).unwrap();
    assert!(log.contains("Merge branch"));
}

#[test]
fn ownership_guard_rejects_unowned_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    // create a worktree outside of GitModule (simulating another session)
    Command::new("git")
        .args(["worktree", "add", "-b", "other/branch",
            tmp.path().join(".worktrees/foreign").to_str().unwrap(), "main"])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let foreign_path = tmp.path().join(".worktrees/foreign");

    // commit to unowned worktree should fail
    std::fs::write(foreign_path.join("file.txt"), "x").unwrap();
    let err = git.commit(&foreign_path, "bad commit", None);
    assert!(err.is_err());
    assert!(err.unwrap_err().to_string().contains("not owned by this session"));

    // branch_delete on unowned branch should fail
    let err = git.branch_delete("other/branch");
    assert!(err.is_err());
    assert!(err.unwrap_err().to_string().contains("not owned by this session"));
}

#[test]
fn commit_allowed_at_session_root() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    std::fs::write(tmp.path().join("root_file.txt"), "content\n").unwrap();
    let result = git.commit(tmp.path(), "root commit", Some(&["root_file.txt".to_string()]));
    assert!(result.is_ok());
    assert!(result.unwrap().contains("committed"));
}
