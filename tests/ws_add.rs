mod common;

use common::{stderr, WorkspaceFixture};

#[test]
fn happy_path_extends_an_existing_task() {
    // The whole point: after `ws new api feat/x`, running `ws add app feat/x`
    // should leave both worktrees in place with no state on `api` disturbed.
    let f = WorkspaceFixture::with_repos(&["api", "app"]);

    f.run_ok(&["new", "api", "feat/x"]);
    assert!(f.path().join(".worktrees/feat-x/api").is_dir());
    assert!(!f.path().join(".worktrees/feat-x/app").exists());

    let out = f.run_ok(&["add", "app", "feat/x"]);
    let err = stderr(&out);

    assert!(f.path().join(".worktrees/feat-x/api").is_dir(), "api worktree must survive add");
    assert!(f.path().join(".worktrees/feat-x/app").is_dir(), "app worktree must exist after add");
    assert!(err.contains("Worktree created"), "expected 'Worktree created' for app in:\n{err}");
    assert!(err.contains("app:"), "expected 'app:' in summary:\n{err}");
    assert!(err.contains("ws up feat/x"), "expected nudge in:\n{err}");
}

#[test]
fn refuses_when_the_task_does_not_exist() {
    let f = WorkspaceFixture::with_repos(&["api"]);

    let out = f.run(&["add", "api", "feat/never-created"]);
    assert_eq!(out.status.code(), Some(2), "missing task is a preflight failure");

    let err = stderr(&out);
    assert!(err.contains("no worktrees for"), "expected 'no worktrees for' in:\n{err}");
    assert!(err.contains("ws new"), "expected 'ws new' nudge in:\n{err}");
}

#[test]
fn refuses_an_unknown_repo() {
    let f = WorkspaceFixture::with_repos(&["api", "app"]);
    f.run_ok(&["new", "api", "feat/x"]);

    let out = f.run(&["add", "bogus", "feat/x"]);
    assert_eq!(out.status.code(), Some(2));

    let err = stderr(&out);
    assert!(err.contains("bogus"), "expected the unknown repo in error:\n{err}");
}

#[test]
fn refuses_a_repo_already_in_the_task() {
    let f = WorkspaceFixture::with_repos(&["api", "app"]);
    f.run_ok(&["new", "api,app", "feat/x"]);

    let out = f.run(&["add", "api", "feat/x"]);
    assert_eq!(out.status.code(), Some(1), "conflict is an op failure, not a preflight");

    let err = stderr(&out);
    assert!(err.contains("already in"), "expected 'already in' in error:\n{err}");
}

#[test]
fn refuses_when_the_task_is_live() {
    // `down → add → up` is the promised order; a repo whose declared status
    // hook says "running" makes the task not-stopped and add must refuse
    // rather than race the new repo's post-create against a live stack.
    let f = WorkspaceFixture::with_repos(&["api", "app"]);
    f.add_hook("api", "status", "echo running");

    f.run_ok(&["new", "api", "feat/x"]);

    let out = f.run(&["add", "app", "feat/x"]);
    assert_eq!(out.status.code(), Some(2), "live task is a preflight failure");

    let err = stderr(&out);
    assert!(err.contains("not fully stopped"), "expected 'not fully stopped' in:\n{err}");
    assert!(err.contains("api"), "expected the live repo named in:\n{err}");
    assert!(err.contains("ws down feat/x"), "expected the down nudge in:\n{err}");

    assert!(
        !f.path().join(".worktrees/feat-x/app").exists(),
        "app worktree must not be created when the preflight refuses",
    );
}

#[test]
fn a_stopped_hook_is_treated_as_stopped_and_add_proceeds() {
    // Regression: a repo with a declared status hook that returns "stopped"
    // must not be misclassified — add should succeed.
    let f = WorkspaceFixture::with_repos(&["api", "app"]);
    f.add_hook("api", "status", "echo stopped");

    f.run_ok(&["new", "api", "feat/x"]);
    f.run_ok(&["add", "app", "feat/x"]);

    assert!(f.path().join(".worktrees/feat-x/app").is_dir());
}

#[test]
fn plain_repo_with_no_hooks_does_not_block_add() {
    // Plain repos (no kind, no scripts) have nothing to be "up" — probe_status
    // returns None and add must treat that as Stopped rather than refusing on
    // unknown, or every default ws.json is a footgun.
    let f = WorkspaceFixture::with_repos(&["api", "app"]);

    f.run_ok(&["new", "api", "feat/x"]);
    f.run_ok(&["add", "app", "feat/x"]);

    assert!(f.path().join(".worktrees/feat-x/api").is_dir());
    assert!(f.path().join(".worktrees/feat-x/app").is_dir());
}
