use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use lds_core::{Session, SessionConfig};
use lds_git::{GitModule, LogFilters, OtherStagedMode, ResetMode};

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

#[tokio::test]
async fn worktree_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let mut git = GitModule::new(session);

    // worktree_list: only main worktree, not owned by us yet.
    let list = git.worktree_list().await.unwrap();
    assert_eq!(list.worktrees.len(), 1);
    assert!(!list.worktrees[0].owned);

    // worktree_add
    let add_result = git
        .worktree_add("test-wt", "feat/test", Some("main"))
        .await
        .unwrap();
    assert!(add_result.path.ends_with("test-wt"));
    assert_eq!(add_result.branch, "feat/test");

    // worktree_list: the new worktree is now owned.
    let list = git.worktree_list().await.unwrap();
    assert!(
        list.worktrees.iter().any(|w| w.owned),
        "expected at least one owned worktree, got: {list:?}"
    );

    // commit in worktree
    let wt_path = tmp.path().join(".worktrees/test-wt");
    std::fs::write(wt_path.join("new_file.txt"), "content\n").unwrap();
    let commit_result = git
        .commit(&wt_path, "test commit", None, OtherStagedMode::Stop, false)
        .await
        .unwrap();
    assert_eq!(commit_result.sha.len(), 40, "expected full SHA-1");
    assert_eq!(commit_result.message, "test commit");
    assert_eq!(commit_result.files_changed, 1);

    // merge back to main
    let merge_result = git.merge("feat/test", "main", tmp.path()).await.unwrap();
    assert_eq!(merge_result.branch, "feat/test");
    assert_eq!(merge_result.into_branch, "main");
    assert_eq!(merge_result.sha.len(), 40);

    // worktree_remove
    let remove_result = git.worktree_remove("test-wt").await.unwrap();
    assert!(remove_result.path.ends_with("test-wt"));

    // branch_delete
    let delete_result = git.branch_delete("feat/test").await.unwrap();
    assert_eq!(delete_result.branch, "feat/test");

    // verify merge landed: new_file.txt should exist in main
    assert!(tmp.path().join("new_file.txt").exists());

    // git log should show the merge commit
    let log = git
        .log(LogFilters {
            max_count: 5,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        log.commits
            .iter()
            .any(|c| c.summary.contains("Merge branch")),
        "expected a 'Merge branch' commit in log, got: {log:?}"
    );
}

#[tokio::test]
async fn log_filters_by_author_paths_and_since() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let run = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git");
    };
    run(&["init", "-b", "main"]);
    run(&["config", "user.email", "alice@example.com"]);
    run(&["config", "user.name", "Alice"]);

    std::fs::write(dir.join("a.txt"), "1\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "-m", "add a.txt"]);

    // Rewrite the second commit under Bob.
    run(&["config", "user.email", "bob@example.com"]);
    run(&["config", "user.name", "Bob"]);
    std::fs::write(dir.join("b.txt"), "2\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "-m", "add b.txt"]);

    // Third commit — Alice again — touches only a.txt.
    run(&["config", "user.email", "alice@example.com"]);
    run(&["config", "user.name", "Alice"]);
    std::fs::write(dir.join("a.txt"), "1\n2\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "-m", "update a.txt"]);

    std::fs::create_dir_all(dir.join(".worktrees")).unwrap();
    let session = make_session(dir);
    let git = GitModule::new(session);

    // author filter — only Bob's commit survives.
    let bob_only = git
        .log(LogFilters {
            max_count: 10,
            author: Some("Bob".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(bob_only.commits.len(), 1);
    assert_eq!(bob_only.commits[0].summary, "add b.txt");
    assert!(bob_only.commits[0].author.contains("bob@example.com"));

    // path filter — commits touching b.txt (only the second).
    let touches_b = git
        .log(LogFilters {
            max_count: 10,
            paths: Some(vec!["b.txt".to_string()]),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(touches_b.commits.len(), 1);
    assert_eq!(touches_b.commits[0].summary, "add b.txt");

    // path filter — commits touching a.txt (first + third).
    let touches_a = git
        .log(LogFilters {
            max_count: 10,
            paths: Some(vec!["a.txt".to_string()]),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(touches_a.commits.len(), 2);

    // since filter — cutoff after the third commit's author time drops all.
    let head_ts = touches_a.commits[0].timestamp;
    let none = git
        .log(LogFilters {
            max_count: 10,
            since: Some(head_ts + 1),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(none.commits.len(), 0);

    // max_count is applied post-filter.
    let capped = git
        .log(LogFilters {
            max_count: 1,
            author: Some("Alice".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(capped.commits.len(), 1);

    // grep filter — matches subject substring only.
    let updates = git
        .log(LogFilters {
            max_count: 10,
            grep: Some("update".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(updates.commits.len(), 1);
    assert_eq!(updates.commits[0].summary, "update a.txt");

    let no_match = git
        .log(LogFilters {
            max_count: 10,
            grep: Some("does-not-appear".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(no_match.commits.len(), 0);

    // metadata fields are populated.
    let head = &touches_a.commits[0];
    assert_eq!(head.sha.len(), 40);
    assert_eq!(head.short_sha.len(), 7);
    assert!(head.author.starts_with("Alice <"));
    assert!(head.timestamp > 0);
}

#[tokio::test]
async fn ownership_guard_rejects_unowned_worktree() {
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
    let err = git
        .commit(
            &foreign_path,
            "bad commit",
            None,
            OtherStagedMode::Stop,
            false,
        )
        .await;
    assert!(err.is_err());
    assert!(
        err.unwrap_err()
            .to_string()
            .contains("not owned by this session")
    );

    // branch_delete on unowned branch should fail
    let err = git.branch_delete("other/branch").await;
    assert!(err.is_err());
    assert!(
        err.unwrap_err()
            .to_string()
            .contains("not owned by this session")
    );
}

#[tokio::test]
async fn commit_allowed_at_session_root() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    std::fs::write(tmp.path().join("root_file.txt"), "content\n").unwrap();
    let result = git
        .commit(
            tmp.path(),
            "root commit",
            Some(&["root_file.txt".to_string()]),
            OtherStagedMode::Stop,
            false,
        )
        .await;
    let commit = result.expect("commit at session root");
    assert_eq!(commit.sha.len(), 40);
    assert_eq!(commit.message, "root commit");
}

#[tokio::test]
async fn status_partitions_staged_unstaged_untracked() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    // Clean state right after the initial commit.
    // The `.worktrees/` directory created by init_temp_repo is itself
    // untracked, so the clean predicate examines staged + unstaged only.
    let status = git.status().await.unwrap();
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
    let status = git.status().await.unwrap();
    assert!(
        status
            .untracked
            .iter()
            .any(|p| p.ends_with("untracked.txt")),
        "untracked was {:?}",
        status.untracked
    );

    // Stage it -> staged bucket only.
    Command::new("git")
        .args(["add", "untracked.txt"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let status = git.status().await.unwrap();
    assert!(
        status
            .staged
            .iter()
            .any(|e| e.path.ends_with("untracked.txt")),
        "staged was {:?}",
        status.staged
    );
}

#[tokio::test]
async fn diff_distinguishes_staged_from_unstaged() {
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

    let unstaged = git.diff(false).await.unwrap();
    assert!(!unstaged.staged);
    assert_eq!(
        unstaged.file_count, 0,
        "expected no unstaged changes, patch was: {:?}",
        unstaged.patch
    );

    let staged = git.diff(true).await.unwrap();
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
    let unstaged = git.diff(false).await.unwrap();
    assert!(!unstaged.staged);
    assert_eq!(unstaged.file_count, 1);
    assert!(
        unstaged.patch.contains("again"),
        "expected '+again' line in unstaged patch, got: {:?}",
        unstaged.patch
    );
}

#[tokio::test]
async fn reset_moves_head_back() {
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
        .reset(tmp.path(), ResetMode::Hard, &before_sha, false)
        .await
        .expect("reset");
    assert!(matches!(result.mode, ResetMode::Hard));
    assert_eq!(result.target, before_sha);
    assert_eq!(result.current_head, before_sha);
    assert_ne!(result.previous_head, result.current_head);
}

#[tokio::test]
async fn session_release_adopts_orphan_worktree() {
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
    assert!(git.branch_delete("left/over").await.is_err());

    // Adopt: session_release should pick up `leftover` + branch `left/over`.
    let release = git.session_release().await.expect("session_release");
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
    git.worktree_remove("leftover")
        .await
        .expect("worktree_remove after adoption");
    git.branch_delete("left/over")
        .await
        .expect("branch_delete after adoption");
}

// ---------------------------------------------------------------------------
// commit(only, other_staged)
// ---------------------------------------------------------------------------

/// `git status --porcelain=v1` line list from `dir`. Test-only helper.
fn porcelain_status(dir: &Path) -> Vec<String> {
    let out = Command::new("git")
        .args(["status", "--porcelain=v1"])
        .current_dir(dir)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.to_string())
        .collect()
}

/// Stage a worktree file so tests can seed "other staged" state.
fn git_add(dir: &Path, path: &str) {
    Command::new("git")
        .args(["add", path])
        .current_dir(dir)
        .output()
        .unwrap();
}

#[tokio::test]
async fn commit_only_commits_just_those_paths() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    // Two new files in the worktree, only one is asked for.
    std::fs::write(tmp.path().join("keep.txt"), "keep\n").unwrap();
    std::fs::write(tmp.path().join("skip.txt"), "skip\n").unwrap();

    let commit = git
        .commit(
            tmp.path(),
            "only keep",
            Some(&["keep.txt".to_string()]),
            OtherStagedMode::Stop,
            false,
        )
        .await
        .expect("commit only=keep.txt");

    assert_eq!(commit.files_changed, 1);
    // The committed tree contains keep.txt but not skip.txt.
    let listed = Command::new("git")
        .args(["show", "--name-only", "--pretty=format:", "HEAD"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let listed = String::from_utf8_lossy(&listed.stdout);
    assert!(
        listed.contains("keep.txt"),
        "commit should touch keep.txt: {listed}"
    );
    assert!(
        !listed.contains("skip.txt"),
        "commit must not touch skip.txt: {listed}"
    );
    // skip.txt is still on disk, untracked (never staged by us).
    assert!(tmp.path().join("skip.txt").exists());
    let status = porcelain_status(tmp.path());
    assert!(
        status.iter().any(|l| l.ends_with("skip.txt")),
        "skip.txt should still be reported by status, got: {status:?}"
    );
}

#[tokio::test]
async fn commit_only_stop_refuses_when_index_has_intruders() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    // Pre-stage `other.txt` outside of `only`. `stop` mode must abort.
    std::fs::write(tmp.path().join("other.txt"), "other\n").unwrap();
    git_add(tmp.path(), "other.txt");

    std::fs::write(tmp.path().join("only.txt"), "only\n").unwrap();

    let err = git
        .commit(
            tmp.path(),
            "should abort",
            Some(&["only.txt".to_string()]),
            OtherStagedMode::Stop,
            false,
        )
        .await
        .expect_err("stop mode must refuse when other paths are staged");
    let msg = err.to_string();
    assert!(
        msg.contains("other_staged=stop"),
        "expected other_staged=stop hint in error: {msg}"
    );
    assert!(
        msg.contains("other.txt"),
        "error should name the intruder: {msg}"
    );

    // State must be untouched: HEAD is still `initial`, other.txt still staged.
    let head_msg = Command::new("git")
        .args(["log", "-1", "--pretty=%s"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&head_msg.stdout).trim(),
        "initial",
        "stop mode must not create a commit"
    );
    let status = porcelain_status(tmp.path());
    assert!(
        status.iter().any(|l| l.starts_with("A  other.txt")),
        "other.txt must remain staged, got: {status:?}"
    );
}

#[tokio::test]
async fn commit_only_restage_survives_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    // Pre-stage `other.txt`; call commit with only=["target.txt"] + restage.
    std::fs::write(tmp.path().join("other.txt"), "other\n").unwrap();
    git_add(tmp.path(), "other.txt");
    std::fs::write(tmp.path().join("target.txt"), "target\n").unwrap();

    let commit = git
        .commit(
            tmp.path(),
            "only target",
            Some(&["target.txt".to_string()]),
            OtherStagedMode::Restage,
            false,
        )
        .await
        .expect("restage mode commits target and re-stages other");

    assert_eq!(commit.files_changed, 1, "commit must only touch target.txt");

    // The new commit contains target.txt, not other.txt.
    let listed = Command::new("git")
        .args(["show", "--name-only", "--pretty=format:", "HEAD"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let listed = String::from_utf8_lossy(&listed.stdout);
    assert!(
        listed.contains("target.txt"),
        "commit should touch target.txt: {listed}"
    );
    assert!(
        !listed.contains("other.txt"),
        "commit must not touch other.txt: {listed}"
    );

    // other.txt has been re-staged (still listed as staged-add by porcelain).
    let status = porcelain_status(tmp.path());
    assert!(
        status.iter().any(|l| l.starts_with("A  other.txt")),
        "other.txt must be re-staged after restage round-trip, got: {status:?}"
    );
}

#[tokio::test]
async fn commit_only_stop_ignores_other_unstaged_changes() {
    // Modified-but-not-staged files are NOT intruders — the mode only cares
    // about the index. This documents that boundary.
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    // Modify README.md but do not stage it.
    std::fs::write(tmp.path().join("README.md"), "init\nunstaged edit\n").unwrap();
    // Add a fresh file that `only` will target.
    std::fs::write(tmp.path().join("target.txt"), "t\n").unwrap();

    let commit = git
        .commit(
            tmp.path(),
            "only target with unstaged noise",
            Some(&["target.txt".to_string()]),
            OtherStagedMode::Stop,
            false,
        )
        .await
        .expect("stop mode should still succeed when noise is only unstaged");
    assert_eq!(commit.files_changed, 1);

    // README's unstaged edit is still on disk, unstaged.
    let status = porcelain_status(tmp.path());
    assert!(
        status.iter().any(|l| l.starts_with(" M README.md")),
        "README.md's unstaged edit must survive, got: {status:?}"
    );
}

// ---------------------------------------------------------------------------
// Dotfile / dot-dir safeguard
// ---------------------------------------------------------------------------

#[tokio::test]
async fn commit_skips_untracked_dotfile_not_in_gitignore() {
    // `.env` newly-dropped without `.gitignore` coverage: the safeguard
    // must drop it from staging AND warn so the caller sees it.
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    std::fs::write(tmp.path().join(".env"), "SECRET=1\n").unwrap();
    std::fs::write(tmp.path().join("safe.txt"), "safe\n").unwrap();

    let commit = git
        .commit(
            tmp.path(),
            "commit with dotfile in tree",
            None,
            OtherStagedMode::Stop,
            false,
        )
        .await
        .expect("commit should succeed with safe.txt only");

    assert_eq!(commit.files_changed, 1, "only safe.txt should be committed");
    assert!(
        commit.dotfile_skipped.iter().any(|p| p == ".env"),
        "expected .env in dotfile_skipped, got {:?}",
        commit.dotfile_skipped
    );
    assert!(
        commit
            .dotfile_warnings
            .iter()
            .any(|w| w.path == ".env" && !w.tracked && !w.in_gitignore),
        "expected untracked+not-ignored .env warning, got {:?}",
        commit.dotfile_warnings
    );

    // `.env` is still untracked on disk — never made it into the index.
    let status = porcelain_status(tmp.path());
    assert!(
        status
            .iter()
            .any(|l| l.starts_with("?? ") && l.ends_with(".env")),
        ".env must remain untracked, got: {status:?}"
    );
}

#[tokio::test]
async fn commit_silently_skips_gitignored_dotfile() {
    // `.env` covered by `.gitignore`: git already hides it from
    // porcelain, so it never even shows up as a candidate. No warning
    // should be emitted for the silent case.
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    std::fs::write(tmp.path().join(".gitignore"), ".env\n").unwrap();
    // Commit .gitignore first so it's tracked and quiet on subsequent runs.
    let _ = git
        .commit(
            tmp.path(),
            "add .gitignore",
            Some(&[".gitignore".to_string()]),
            OtherStagedMode::Stop,
            true, // force_dot: bootstrap the safeguard itself
        )
        .await
        .expect("bootstrap .gitignore");

    std::fs::write(tmp.path().join(".env"), "SECRET=1\n").unwrap();
    std::fs::write(tmp.path().join("safe.txt"), "safe\n").unwrap();

    let commit = git
        .commit(
            tmp.path(),
            "commit with ignored .env in tree",
            None,
            OtherStagedMode::Stop,
            false,
        )
        .await
        .expect("commit safe.txt only");

    assert_eq!(commit.files_changed, 1);
    assert!(
        commit.dotfile_warnings.is_empty(),
        "ignored .env must not warn, got {:?}",
        commit.dotfile_warnings
    );
    assert!(
        commit.dotfile_skipped.is_empty(),
        "ignored .env is already invisible to porcelain, got {:?}",
        commit.dotfile_skipped
    );
}

#[tokio::test]
async fn commit_warns_but_includes_tracked_dotfile_change() {
    // Modify a tracked `.gitignore`: safeguard lets the change through
    // (tracked = intentional) but records a warning so pre-publish
    // review can catch unintended edits.
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    std::fs::write(tmp.path().join(".gitignore"), "old\n").unwrap();
    let _ = git
        .commit(
            tmp.path(),
            "add .gitignore",
            Some(&[".gitignore".to_string()]),
            OtherStagedMode::Stop,
            true,
        )
        .await
        .expect("bootstrap tracked .gitignore");

    // Now edit it.
    std::fs::write(tmp.path().join(".gitignore"), "old\nnew line\n").unwrap();

    let commit = git
        .commit(
            tmp.path(),
            "update .gitignore",
            None,
            OtherStagedMode::Stop,
            false,
        )
        .await
        .expect("commit tracked dotfile change");

    assert_eq!(commit.files_changed, 1, ".gitignore edit must be committed");
    assert!(
        commit
            .dotfile_warnings
            .iter()
            .any(|w| w.path == ".gitignore" && w.tracked),
        "expected tracked .gitignore warning, got {:?}",
        commit.dotfile_warnings
    );
    assert!(
        commit.dotfile_skipped.is_empty(),
        "tracked dotfile must not appear in skipped, got {:?}",
        commit.dotfile_skipped
    );
}

#[tokio::test]
async fn commit_force_dot_suppresses_safeguard() {
    // `force_dot=true`: `.env` gets staged and committed like any other
    // file, no warning, no skip. This is the manual override path.
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    std::fs::write(tmp.path().join(".env"), "SECRET=1\n").unwrap();

    let commit = git
        .commit(
            tmp.path(),
            "force-commit .env",
            None,
            OtherStagedMode::Stop,
            true,
        )
        .await
        .expect("force_dot must let .env through");

    assert_eq!(commit.files_changed, 1);
    assert!(
        commit.dotfile_warnings.is_empty(),
        "force_dot must suppress warnings, got {:?}",
        commit.dotfile_warnings
    );
    assert!(
        commit.dotfile_skipped.is_empty(),
        "force_dot must suppress skip list, got {:?}",
        commit.dotfile_skipped
    );

    // .env is now tracked at HEAD.
    let listed = Command::new("git")
        .args(["show", "--name-only", "--pretty=format:", "HEAD"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let listed = String::from_utf8_lossy(&listed.stdout);
    assert!(
        listed.contains(".env"),
        "force_dot commit should include .env: {listed}"
    );
}

#[tokio::test]
async fn commit_only_drops_dotfile_from_explicit_paths() {
    // `only=[.env, safe.txt]` with safeguard on: `.env` is filtered
    // out of the stage list, `safe.txt` proceeds. HEAD contains only
    // safe.txt afterward.
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    std::fs::write(tmp.path().join(".env"), "SECRET=1\n").unwrap();
    std::fs::write(tmp.path().join("safe.txt"), "safe\n").unwrap();

    let commit = git
        .commit(
            tmp.path(),
            "explicit paths with dotfile filtered",
            Some(&[".env".to_string(), "safe.txt".to_string()]),
            OtherStagedMode::Stop,
            false,
        )
        .await
        .expect("commit should proceed with safe.txt only");

    assert_eq!(commit.files_changed, 1);
    assert!(
        commit.dotfile_skipped.iter().any(|p| p == ".env"),
        "expected .env skipped, got {:?}",
        commit.dotfile_skipped
    );

    let listed = Command::new("git")
        .args(["show", "--name-only", "--pretty=format:", "HEAD"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let listed = String::from_utf8_lossy(&listed.stdout);
    assert!(listed.contains("safe.txt"));
    assert!(
        !listed.contains(".env"),
        "safeguard-filtered .env must not land in commit: {listed}"
    );
}

#[tokio::test]
async fn commit_detects_dotfile_under_untracked_nondot_dir() {
    // Regression: previously the safeguard missed `foo/.hidden` because
    // `git status --porcelain` collapsed the untracked `foo/` into a
    // single `?? foo/` record, so the dotfile-classifier only ever saw
    // `foo/` (non-dot) and let git recursively stage everything under it.
    // Fixed by `--untracked-files=all`. Origin: 2026-07-22 jikki smoke
    // that saw `session_start`'s `workspace/.journal.db*` slip through.
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    // Untracked non-dot dir with an untracked nested dotfile.
    std::fs::create_dir_all(tmp.path().join("data")).unwrap();
    std::fs::write(tmp.path().join("data/.hidden"), "secret\n").unwrap();
    std::fs::write(tmp.path().join("data/public.txt"), "ok\n").unwrap();

    let commit = git
        .commit(
            tmp.path(),
            "nested dotfile under untracked non-dot dir",
            None,
            OtherStagedMode::Stop,
            false,
        )
        .await
        .expect("commit should proceed with data/public.txt only");

    assert!(
        commit.dotfile_skipped.iter().any(|p| p == "data/.hidden"),
        "expected data/.hidden in dotfile_skipped, got {:?}",
        commit.dotfile_skipped
    );
    assert!(
        commit
            .dotfile_warnings
            .iter()
            .any(|w| w.path == "data/.hidden" && !w.tracked),
        "expected untracked warning for data/.hidden, got {:?}",
        commit.dotfile_warnings
    );

    // HEAD must contain data/public.txt but NOT data/.hidden.
    let listed = Command::new("git")
        .args(["show", "--name-only", "--pretty=format:", "HEAD"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let listed = String::from_utf8_lossy(&listed.stdout);
    assert!(
        listed.contains("data/public.txt"),
        "commit should include data/public.txt: {listed}"
    );
    assert!(
        !listed.contains(".hidden"),
        "safeguard-filtered data/.hidden must not land in commit: {listed}"
    );

    // data/.hidden is still on disk, untracked.
    let status = porcelain_status(tmp.path());
    assert!(
        status.iter().any(|l| l.ends_with("data/.hidden")),
        "data/.hidden must remain untracked, got: {status:?}"
    );
}

#[tokio::test]
async fn commit_detects_nested_dotdir_component() {
    // `.config/tool.toml`: dot component is not the basename but a
    // path segment. is_dotfile_path must still catch it.
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    std::fs::create_dir_all(tmp.path().join(".config")).unwrap();
    std::fs::write(tmp.path().join(".config/tool.toml"), "hi\n").unwrap();
    std::fs::write(tmp.path().join("safe.txt"), "safe\n").unwrap();

    let commit = git
        .commit(
            tmp.path(),
            "nested dot-dir",
            None,
            OtherStagedMode::Stop,
            false,
        )
        .await
        .expect("commit safe.txt only");

    assert_eq!(commit.files_changed, 1);
    assert!(
        commit
            .dotfile_skipped
            .iter()
            .any(|p| p.starts_with(".config/")),
        "expected .config/... in skipped, got {:?}",
        commit.dotfile_skipped
    );
}

// ---------------------------------------------------------------------------
// Stash transaction (apply / abort / finalize)
// ---------------------------------------------------------------------------

/// Run `git <args>` in `dir`, asserting success. Test-only helper for seeding
/// stash state the way a human would (`git stash push` is deliberately not
/// exposed by the module under test).
fn git_run(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn read(dir: &Path, rel: &str) -> String {
    std::fs::read_to_string(dir.join(rel)).unwrap()
}

#[tokio::test]
async fn stash_list_reports_entries_and_untracked_flag() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    // Entry 1: tracked modification only.
    std::fs::write(tmp.path().join("README.md"), "init\ntracked edit\n").unwrap();
    git_run(tmp.path(), &["stash", "push", "-m", "wip tracked"]);

    // Entry 2: untracked file, stashed with -u (3rd parent).
    std::fs::write(tmp.path().join("u.txt"), "untracked\n").unwrap();
    git_run(tmp.path(), &["stash", "push", "-u", "-m", "wip untracked"]);

    let list = git.stash_list().await.expect("stash_list");
    assert_eq!(list.stashes.len(), 2, "got: {list:?}");

    // Newest first: index 0 is the -u entry.
    assert_eq!(list.stashes[0].index, 0);
    assert_eq!(list.stashes[0].sha.len(), 40, "expected full SHA-1");
    assert!(
        list.stashes[0].message.contains("wip untracked"),
        "got: {:?}",
        list.stashes[0].message
    );
    assert!(
        list.stashes[0].has_untracked,
        "stash push -u must set has_untracked, got: {:?}",
        list.stashes[0]
    );

    assert_eq!(list.stashes[1].index, 1);
    assert!(
        !list.stashes[1].has_untracked,
        "tracked-only stash must not claim untracked, got: {:?}",
        list.stashes[1]
    );
}

#[tokio::test]
async fn stash_show_reports_patch_files_and_untracked_paths() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    std::fs::write(tmp.path().join("README.md"), "init\nstashed line\n").unwrap();
    std::fs::create_dir_all(tmp.path().join("nested")).unwrap();
    std::fs::write(tmp.path().join("nested/u.txt"), "untracked\n").unwrap();
    git_run(tmp.path(), &["stash", "push", "-u", "-m", "wip both"]);

    let show = git.stash_show(0).await.expect("stash_show");
    assert_eq!(show.index, 0);
    assert_eq!(show.sha.len(), 40);
    assert_eq!(show.file_count, 1, "only README.md is a tracked change");
    assert_eq!(show.files, vec!["README.md".to_string()]);
    assert!(
        show.patch.contains("+stashed line"),
        "patch must carry the stashed hunk, got: {}",
        show.patch
    );
    assert_eq!(
        show.untracked_paths,
        vec!["nested/u.txt".to_string()],
        "untracked snapshot must be enumerated with its full relative path"
    );

    // Nothing was applied by the read path.
    assert_eq!(read(tmp.path(), "README.md"), "init\n");
    assert!(!tmp.path().join("nested/u.txt").exists());
}

#[tokio::test]
async fn stash_apply_refuses_dirty_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    std::fs::write(tmp.path().join("README.md"), "init\nstashed\n").unwrap();
    git_run(tmp.path(), &["stash", "push", "-m", "wip"]);

    // Hand edit on top: mixing this with an apply would make abort unsafe.
    std::fs::write(tmp.path().join("other.txt"), "hand edit\n").unwrap();
    git_add(tmp.path(), "other.txt");
    std::fs::write(tmp.path().join("README.md"), "init\nhand edit\n").unwrap();

    let err = git
        .stash_apply(tmp.path(), 0, None)
        .await
        .expect_err("dirty worktree must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("working tree must be clean"),
        "expected clean-tree refusal, got: {msg}"
    );
    assert!(
        msg.contains("other.txt") && msg.contains("README.md"),
        "refusal must name the offending paths, got: {msg}"
    );

    // Refusal is a no-op: the entry is untouched and so is the hand edit.
    assert_eq!(git.stash_list().await.unwrap().stashes.len(), 1);
    assert_eq!(read(tmp.path(), "README.md"), "init\nhand edit\n");
}

#[tokio::test]
async fn stash_apply_restores_content_and_keeps_entry() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    std::fs::write(tmp.path().join("README.md"), "init\nstashed\n").unwrap();
    std::fs::write(tmp.path().join("u.txt"), "untracked\n").unwrap();
    git_run(tmp.path(), &["stash", "push", "-u", "-m", "wip"]);
    let sha = git.stash_list().await.unwrap().stashes[0].sha.clone();

    let applied = git
        .stash_apply(tmp.path(), 0, Some(sha.clone()))
        .await
        .expect("apply on a clean tree");

    assert_eq!(applied.index, 0);
    assert_eq!(applied.sha, sha);
    assert!(applied.entry_kept);
    assert_eq!(applied.applied_paths, vec!["README.md".to_string()]);
    assert_eq!(applied.restored_untracked, vec!["u.txt".to_string()]);

    assert_eq!(read(tmp.path(), "README.md"), "init\nstashed\n");
    assert_eq!(read(tmp.path(), "u.txt"), "untracked\n");

    // The entry survives — that's what makes finalize a separate decision.
    let list = git.stash_list().await.unwrap();
    assert_eq!(list.stashes.len(), 1, "apply must not drop, got: {list:?}");
    assert_eq!(list.stashes[0].sha, sha);
}

#[tokio::test]
async fn stash_apply_refuses_existing_untracked_collision() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    std::fs::write(tmp.path().join("u.txt"), "stashed untracked\n").unwrap();
    git_run(tmp.path(), &["stash", "push", "-u", "-m", "wip"]);

    // Same path reappears — git would abort part-way through the apply.
    std::fs::write(tmp.path().join("u.txt"), "current untracked\n").unwrap();

    let err = git
        .stash_apply(tmp.path(), 0, None)
        .await
        .expect_err("untracked collision must be refused");
    assert!(
        err.to_string().contains("u.txt"),
        "refusal must name the colliding path, got: {err}"
    );
    assert_eq!(read(tmp.path(), "u.txt"), "current untracked\n");
    assert_eq!(git.stash_list().await.unwrap().stashes.len(), 1);
}

#[tokio::test]
async fn stash_apply_rejects_mismatched_expected_sha() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    std::fs::write(tmp.path().join("README.md"), "init\nstashed\n").unwrap();
    git_run(tmp.path(), &["stash", "push", "-m", "wip"]);

    let err = git
        .stash_apply(tmp.path(), 0, Some("0".repeat(40)))
        .await
        .expect_err("sha mismatch must be refused");
    assert!(
        err.to_string().contains("stash index shifted"),
        "expected an index-shift diagnosis, got: {err}"
    );
    // Nothing applied.
    assert_eq!(read(tmp.path(), "README.md"), "init\n");

    // A too-short prefix is rejected rather than silently accepted.
    let err = git
        .stash_apply(tmp.path(), 0, Some("abc".to_string()))
        .await
        .expect_err("short sha must be refused");
    assert!(
        err.to_string().contains("too short"),
        "expected a length complaint, got: {err}"
    );
}

#[tokio::test]
async fn stash_abort_returns_worktree_to_head() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    // Stash carries: a modified tracked file, a newly added tracked file,
    // and an untracked file — abort must undo all three shapes.
    std::fs::write(tmp.path().join("README.md"), "init\nstashed\n").unwrap();
    std::fs::write(tmp.path().join("added.txt"), "added\n").unwrap();
    git_add(tmp.path(), "added.txt");
    std::fs::write(tmp.path().join("u.txt"), "untracked\n").unwrap();
    git_run(tmp.path(), &["stash", "push", "-u", "-m", "wip"]);

    let sha = git.stash_list().await.unwrap().stashes[0].sha.clone();
    git.stash_apply(tmp.path(), 0, Some(sha.clone()))
        .await
        .expect("apply");
    assert!(tmp.path().join("added.txt").exists());

    let aborted = git
        .stash_abort(tmp.path(), 0, Some(sha.clone()))
        .await
        .expect("abort");
    assert_eq!(aborted.sha, sha);
    assert!(aborted.entry_kept);
    assert!(
        aborted.reverted_paths.iter().any(|p| p == "README.md")
            && aborted.reverted_paths.iter().any(|p| p == "added.txt"),
        "both tracked shapes must be reported, got: {:?}",
        aborted.reverted_paths
    );
    assert_eq!(aborted.removed_untracked, vec!["u.txt".to_string()]);

    // Worktree is back at HEAD: modification undone, added file gone,
    // restored untracked file gone.
    assert_eq!(read(tmp.path(), "README.md"), "init\n");
    assert!(
        !tmp.path().join("added.txt").exists(),
        "a file the stash added must not survive the abort"
    );
    assert!(!tmp.path().join("u.txt").exists());
    assert!(
        porcelain_status(tmp.path()).is_empty(),
        "abort must leave a clean tree, got: {:?}",
        porcelain_status(tmp.path())
    );

    // Entry untouched — abort undoes the apply, it does not discard work.
    let list = git.stash_list().await.unwrap();
    assert_eq!(list.stashes.len(), 1);
    assert_eq!(list.stashes[0].sha, sha);
}

#[tokio::test]
async fn stash_finalize_drops_entry_and_reports_recovery_sha() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    std::fs::write(tmp.path().join("README.md"), "init\nstashed\n").unwrap();
    git_run(tmp.path(), &["stash", "push", "-m", "wip"]);
    let entry = git.stash_list().await.unwrap().stashes[0].clone();

    git.stash_apply(tmp.path(), 0, Some(entry.sha.clone()))
        .await
        .expect("apply");
    let finalized = git
        .stash_finalize(tmp.path(), 0, Some(entry.sha.clone()))
        .await
        .expect("finalize");

    assert_eq!(finalized.index, 0);
    assert_eq!(
        finalized.dropped_sha, entry.sha,
        "dropped_sha must be the pre-drop sha, i.e. the recovery key"
    );
    assert_eq!(finalized.message, entry.message);

    let list = git.stash_list().await.unwrap();
    assert!(list.stashes.is_empty(), "entry must be gone, got: {list:?}");

    // The applied content stays in the working tree.
    assert_eq!(read(tmp.path(), "README.md"), "init\nstashed\n");

    // dropped_sha is still reachable — "dropped", not "lost".
    let show = Command::new("git")
        .args(["show", "--pretty=format:%H", "-s", &finalized.dropped_sha])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(
        show.status.success(),
        "dropped stash commit must remain reachable until gc: {}",
        String::from_utf8_lossy(&show.stderr)
    );
}

#[tokio::test]
async fn stash_apply_conflict_rolls_back_and_keeps_entry() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    // Stash one version of README, then commit a diverging version so the
    // apply is guaranteed to conflict.
    std::fs::write(tmp.path().join("README.md"), "stash version\n").unwrap();
    git_run(tmp.path(), &["stash", "push", "-m", "wip"]);
    std::fs::write(tmp.path().join("README.md"), "head version\n").unwrap();
    git_run(tmp.path(), &["add", "README.md"]);
    git_run(tmp.path(), &["commit", "-m", "diverge"]);

    let sha = git.stash_list().await.unwrap().stashes[0].sha.clone();

    let err = git
        .stash_apply(tmp.path(), 0, Some(sha.clone()))
        .await
        .expect_err("conflicting apply must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("rolled back"),
        "error must state the rollback happened, got: {msg}"
    );
    assert!(
        msg.contains("intact"),
        "error must state the entry survived, got: {msg}"
    );

    // Rollback is total: no conflict markers, no unmerged index entries.
    assert_eq!(read(tmp.path(), "README.md"), "head version\n");
    assert!(
        porcelain_status(tmp.path()).is_empty(),
        "failed apply must leave a clean tree, got: {:?}",
        porcelain_status(tmp.path())
    );

    let list = git.stash_list().await.unwrap();
    assert_eq!(list.stashes.len(), 1, "entry must survive, got: {list:?}");
    assert_eq!(list.stashes[0].sha, sha);
}

#[tokio::test]
async fn reset_hard_refuses_while_a_stash_apply_is_in_flight() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    std::fs::write(tmp.path().join("README.md"), "init\nstashed\n").unwrap();
    git_run(tmp.path(), &["stash", "push", "-m", "wip"]);
    let sha = git.stash_list().await.unwrap().stashes[0].sha.clone();
    git.stash_apply(tmp.path(), 0, Some(sha))
        .await
        .expect("apply");

    // Stash entries + dirty tree == "an apply is in flight": --hard here
    // would erase the applied work with no undo.
    let err = git
        .reset(tmp.path(), ResetMode::Hard, "HEAD", false)
        .await
        .expect_err("hard reset must be guarded");
    let msg = err.to_string();
    assert!(
        msg.contains("git_stash_abort"),
        "guard must point at the non-destructive undo, got: {msg}"
    );
    assert_eq!(read(tmp.path(), "README.md"), "init\nstashed\n");

    // Soft / mixed are unaffected by the guard.
    git.reset(tmp.path(), ResetMode::Mixed, "HEAD", false)
        .await
        .expect("mixed reset is not guarded");
    assert_eq!(read(tmp.path(), "README.md"), "init\nstashed\n");

    // force=true is the deliberate override.
    git.reset(tmp.path(), ResetMode::Hard, "HEAD", true)
        .await
        .expect("force must bypass the guard");
    assert_eq!(read(tmp.path(), "README.md"), "init\n");
}

#[tokio::test]
async fn reset_hard_unguarded_when_no_stash_exists() {
    // The guard keys on stash presence — a plain repo must keep the old
    // behaviour without needing force=true.
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    std::fs::write(tmp.path().join("README.md"), "init\ndirty\n").unwrap();
    git.reset(tmp.path(), ResetMode::Hard, "HEAD", false)
        .await
        .expect("no stash entries -> no guard");
    assert_eq!(read(tmp.path(), "README.md"), "init\n");
}

// ---------------------------------------------------------------------------
// Stash restore (undo of a finalize)
// ---------------------------------------------------------------------------

/// Trimmed stdout of `git <args>` in `dir`. Test-only helper.
fn git_capture(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[tokio::test]
async fn stash_restore_completes_the_apply_finalize_restore_cycle() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    std::fs::write(tmp.path().join("README.md"), "init\nstashed\n").unwrap();
    git_run(tmp.path(), &["stash", "push", "-m", "wip"]);
    let original = git.stash_list().await.unwrap().stashes[0].clone();

    // Full transaction: apply, verify, drop.
    git.stash_apply(tmp.path(), 0, Some(original.sha.clone()))
        .await
        .expect("apply");
    let finalized = git
        .stash_finalize(tmp.path(), 0, Some(original.sha.clone()))
        .await
        .expect("finalize");
    assert!(git.stash_list().await.unwrap().stashes.is_empty());

    // The caller decides the drop was wrong. Put the tree back first so the
    // re-apply below has the clean tree it requires.
    git_run(tmp.path(), &["checkout", "--", "README.md"]);

    let restored = git
        .stash_restore(tmp.path(), &finalized.dropped_sha, None)
        .await
        .expect("restore from dropped_sha");
    assert_eq!(restored.restored_sha, original.sha);
    assert_eq!(restored.index, 0, "store pushes onto the top of the list");
    assert_eq!(
        restored.message, original.message,
        "the entry must come back under the name it had"
    );

    // Back in the list, addressable exactly as before.
    let list = git.stash_list().await.unwrap();
    assert_eq!(list.stashes.len(), 1, "got: {list:?}");
    assert_eq!(list.stashes[0].index, 0);
    assert_eq!(list.stashes[0].sha, original.sha);
    assert_eq!(list.stashes[0].message, original.message);

    // And it still restores the same content.
    assert_eq!(read(tmp.path(), "README.md"), "init\n");
    git.stash_apply(tmp.path(), 0, Some(original.sha.clone()))
        .await
        .expect("re-apply the restored entry");
    assert_eq!(read(tmp.path(), "README.md"), "init\nstashed\n");
}

#[tokio::test]
async fn stash_restore_rejects_a_non_stash_commit() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    // HEAD is an ordinary commit — storing it would create an entry whose
    // "apply" means something nobody asked for.
    let head_sha = git_capture(tmp.path(), &["rev-parse", "HEAD"]);
    let err = git
        .stash_restore(tmp.path(), &head_sha, None)
        .await
        .expect_err("an ordinary commit must be refused");
    assert!(
        err.to_string().contains("not a stash commit"),
        "expected a shape complaint, got: {err}"
    );
    assert!(git.stash_list().await.unwrap().stashes.is_empty());
}

#[tokio::test]
async fn stash_restore_rejects_an_entry_already_in_the_list() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    std::fs::write(tmp.path().join("README.md"), "init\nstashed\n").unwrap();
    git_run(tmp.path(), &["stash", "push", "-m", "wip"]);
    let sha = git.stash_list().await.unwrap().stashes[0].sha.clone();

    let err = git
        .stash_restore(tmp.path(), &sha, None)
        .await
        .expect_err("a live entry must not be duplicated");
    assert!(
        err.to_string().contains("already present at stash@{0}"),
        "expected a duplicate complaint naming the index, got: {err}"
    );
    assert_eq!(
        git.stash_list().await.unwrap().stashes.len(),
        1,
        "refusal must not add a second reference to the same content"
    );
}

#[tokio::test]
async fn stash_restore_rejects_malformed_shas() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    let err = git
        .stash_restore(tmp.path(), "abc123", None)
        .await
        .expect_err("a 6-char sha must be refused");
    assert!(
        err.to_string().contains("too short"),
        "expected a length complaint, got: {err}"
    );

    // Long enough, but a revspec rather than an object id.
    let err = git
        .stash_restore(tmp.path(), "refs/heads/main", None)
        .await
        .expect_err("a revspec must be refused");
    assert!(
        err.to_string().contains("not an object id"),
        "expected a revspec complaint, got: {err}"
    );
}

#[tokio::test]
async fn stash_restore_reports_a_pruned_sha_as_unresolvable() {
    let tmp = tempfile::tempdir().unwrap();
    init_temp_repo(tmp.path());
    let session = make_session(tmp.path());
    let git = GitModule::new(session);

    let err = git
        .stash_restore(tmp.path(), &"f".repeat(40), None)
        .await
        .expect_err("an absent object must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("cannot resolve"),
        "expected a resolution failure, got: {msg}"
    );
    assert!(
        msg.contains("git gc"),
        "error must name gc as the likely cause, got: {msg}"
    );
}
