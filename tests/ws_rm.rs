mod common;

use common::{stderr, WorkspaceFixture};

#[test]
fn rm_on_a_remote_workspace_terminates_the_sync_session() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(
        r#"{"repos":["api"],"remote":{"connection":"srv","workspace_path":"workspace"}}"#,
    );
    let mg = f.arg_recorder("fake-mutagen");

    // Create the worktree (also runs `sync create`, recorded elsewhere).
    let create_args = f.path().join("create-args.txt");
    f.run_with_env(
        &["new", "api", "feat/foo"],
        &[("WS_MUTAGEN", mg.to_str().unwrap()), ("ARGS_FILE", create_args.to_str().unwrap())],
    );

    // Tear it down — should terminate the same session.
    let term_args = f.path().join("term-args.txt");
    let out = f.run_with_env(
        &["rm", "feat/foo"],
        &[("WS_MUTAGEN", mg.to_str().unwrap()), ("ARGS_FILE", term_args.to_str().unwrap())],
    );
    assert!(out.status.success(), "ws rm failed:\n{}", stderr(&out));

    let rec = std::fs::read_to_string(&term_args).expect("mutagen was invoked on rm");
    assert!(rec.contains("terminate"), "expected `sync terminate`:\n{rec}");
    assert!(rec.contains("ws-feat-foo-api"), "expected the session name:\n{rec}");
}

#[test]
fn happy_path_clean_teardown() {
    let f = WorkspaceFixture::with_repos(&["api", "app"]);
    f.run_ok(&["new", "api,app", "feat/gone"]);
    assert!(f.path().join(".worktrees/feat-gone/api").is_dir());

    let out = f.run_ok(&["rm", "feat/gone"]);
    let err = stderr(&out);

    // Worktrees are gone, task dir cleaned up.
    assert!(!f.path().join(".worktrees/feat-gone/api").exists());
    assert!(!f.path().join(".worktrees/feat-gone/app").exists());
    assert!(!f.path().join(".worktrees/feat-gone").exists());
    assert!(err.contains("Removed worktrees"), "expected summary in:\n{err}");

    // Local branch deleted from the main checkouts.
    let out = f.run_ok(&["list"]);
    assert!(stderr(&out).contains("no worktrees"));
}

#[test]
fn dirty_without_force_blocks() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/dirty"]);

    // Track then modify a file so it shows as a real (non-untracked) change.
    let wt = f.path().join(".worktrees/feat-dirty/api");
    std::fs::write(wt.join("tracked.txt"), "v1").unwrap();
    f.git(".worktrees/feat-dirty/api", &["add", "tracked.txt"]);
    f.git(".worktrees/feat-dirty/api", &["commit", "-q", "-m", "add"]);
    std::fs::write(wt.join("tracked.txt"), "v2").unwrap();

    let out = f.run(&["rm", "feat/dirty"]);
    assert_eq!(out.status.code(), Some(1), "expected exit 1 for dirty worktree");

    let err = stderr(&out);
    assert!(err.contains("uncommitted changes"), "expected dirty warning in:\n{err}");
    assert!(err.contains("tracked.txt"), "expected modified file listed in:\n{err}");
    assert!(err.contains("--force"), "expected --force hint in:\n{err}");

    // No teardown happened.
    assert!(wt.is_dir(), "worktree should survive a blocked rm");
}

#[test]
fn dirty_with_force_tears_down() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/forced"]);

    let wt = f.path().join(".worktrees/feat-forced/api");
    std::fs::write(wt.join("tracked.txt"), "v1").unwrap();
    f.git(".worktrees/feat-forced/api", &["add", "tracked.txt"]);
    f.git(".worktrees/feat-forced/api", &["commit", "-q", "-m", "add"]);
    std::fs::write(wt.join("tracked.txt"), "v2").unwrap();

    let out = f.run_ok(&["rm", "feat/forced", "--force"]);
    let err = stderr(&out);

    assert!(err.contains("uncommitted changes"), "expected override warning in:\n{err}");
    assert!(!wt.exists(), "worktree should be removed with --force");
}

#[test]
fn unpushed_without_force_blocks() {
    let f = WorkspaceFixture::empty();
    f.add_repo_with_origin("api");
    f.write_ws_json(r#"{"repos":["api"]}"#);

    // Push a feature branch so a new worktree on it has an upstream.
    f.git("api", &["checkout", "-q", "-b", "feat/up"]);
    f.git("api", &["commit", "-q", "--allow-empty", "-m", "base"]);
    f.git("api", &["push", "-q", "-u", "origin", "feat/up"]);
    f.git("api", &["checkout", "-q", "main"]);

    f.run_ok(&["new", "api", "feat/up"]);

    // Commit locally in the worktree — now ahead of origin/feat/up.
    let wt_rel = ".worktrees/feat-up/api";
    f.git(wt_rel, &["commit", "-q", "--allow-empty", "-m", "local work"]);

    let out = f.run(&["rm", "feat/up"]);
    assert_eq!(out.status.code(), Some(1), "expected exit 1 for unpushed commits");

    let err = stderr(&out);
    assert!(err.contains("unpushed commits"), "expected unpushed warning in:\n{err}");
    assert!(err.contains("local work"), "expected unpushed commit listed in:\n{err}");
}

#[test]
fn no_worktrees_is_preflight_error() {
    let f = WorkspaceFixture::with_repos(&["api"]);

    let out = f.run(&["rm", "feat/nonexistent"]);
    assert_eq!(out.status.code(), Some(2), "expected exit 2 when no worktrees");

    let err = stderr(&out);
    assert!(err.contains("no worktrees"), "expected 'no worktrees' in:\n{err}");
}

#[test]
fn hook_failure_still_removes_but_exits_nonzero() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.add_hook("api", "post-remove", "echo 'cleanup failing'; exit 1");
    f.run_ok(&["new", "api", "feat/hookfail"]);

    let wt = f.path().join(".worktrees/feat-hookfail/api");
    let out = f.run(&["rm", "feat/hookfail"]);

    assert_eq!(out.status.code(), Some(1), "expected exit 1 when a hook fails");
    // Worktree is still removed despite the hook failure.
    assert!(!wt.exists(), "worktree should be removed even when a hook fails");

    let err = stderr(&out);
    assert!(err.contains("hooks failed"), "expected failure summary in:\n{err}");
}

#[test]
fn down_and_post_remove_hooks_run_in_order() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.add_hook("api", "down", "echo 'DOWN-HOOK-RAN'");
    f.add_hook("api", "post-remove", "echo 'POST-REMOVE-RAN'");
    f.run_ok(&["new", "api", "feat/hooks"]);

    let out = f.run_ok(&["rm", "feat/hooks"]);
    let err = stderr(&out);

    let down_at = err.find("DOWN-HOOK-RAN").expect("down hook should run");
    let post_at = err.find("POST-REMOVE-RAN").expect("post-remove hook should run");
    assert!(down_at < post_at, "down should run before post-remove:\n{err}");
}

#[test]
fn leftover_file_in_task_dir_does_not_fail() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/leftover"]);

    // A stray file in the task dir prevents rmdir — op should still succeed.
    std::fs::write(
        f.path().join(".worktrees/feat-leftover/.DS_Store"),
        "junk",
    )
    .unwrap();

    let out = f.run_ok(&["rm", "feat/leftover"]);
    let err = stderr(&out);
    assert!(err.contains("Removed worktrees"), "expected success summary in:\n{err}");
    // Worktree removed even though the task dir couldn't be pruned.
    assert!(!f.path().join(".worktrees/feat-leftover/api").exists());
}
