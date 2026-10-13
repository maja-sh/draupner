mod common;

use common::{stderr, WorkspaceFixture};

#[test]
fn happy_path_both_hooks_run() {
    let f = WorkspaceFixture::with_repos(&["api", "app"]);
    f.add_hook("api", "down", "echo API-DOWN-RAN");
    f.add_hook("app", "down", "echo APP-DOWN-RAN");
    f.run_ok(&["new", "api,app", "feat/foo"]);

    let out = f.run_ok(&["down", "feat/foo"]);
    let err = stderr(&out);

    assert!(err.contains("API-DOWN-RAN"), "expected api hook output in:\n{err}");
    assert!(err.contains("APP-DOWN-RAN"), "expected app hook output in:\n{err}");
    assert_eq!(err.matches("down hook").count(), 2, "expected two down-hook steps in:\n{err}");
    // Symmetric-counterpart nudge back to `wsx up`.
    assert!(err.contains("wsx up feat/foo"), "expected resume nudge in:\n{err}");
}

#[test]
fn partial_only_repos_with_worktrees_run() {
    let f = WorkspaceFixture::with_repos(&["api", "app"]);
    f.add_hook("api", "down", "echo API-DOWN-RAN");
    f.add_hook("app", "down", "echo APP-DOWN-RAN");
    f.run_ok(&["new", "api", "feat/partial"]);

    let out = f.run_ok(&["down", "feat/partial"]);
    let err = stderr(&out);

    assert!(err.contains("API-DOWN-RAN"), "expected api hook output in:\n{err}");
    assert!(!err.contains("APP-DOWN-RAN"), "app has no worktree; its hook must not run:\n{err}");
}

#[test]
fn no_worktrees_is_preflight_error() {
    let f = WorkspaceFixture::with_repos(&["api"]);

    let out = f.run(&["down", "feat/none"]);
    assert_eq!(out.status.code(), Some(2), "expected exit 2 when no worktrees");
    assert!(stderr(&out).contains("no worktrees"));
}

#[test]
fn failure_isolation_second_repo_hook_fails() {
    let f = WorkspaceFixture::with_repos(&["api", "app"]);
    f.add_hook("api", "down", "echo API-DOWN-RAN");
    f.add_hook("app", "down", "echo APP-DOWN-TRIED; exit 1");
    f.run_ok(&["new", "api,app", "feat/iso"]);

    let out = f.run(&["down", "feat/iso"]);
    assert_eq!(out.status.code(), Some(1), "expected exit 1 when a hook fails");
    let err = stderr(&out);

    assert!(err.contains("API-DOWN-RAN"), "first repo should still run:\n{err}");
    assert!(err.contains("APP-DOWN-TRIED"), "second repo hook should have been attempted:\n{err}");
    assert!(err.contains("FAIL"), "expected a failed step line in:\n{err}");
}

#[test]
fn idempotent_second_run_also_succeeds() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.add_hook("api", "down", "echo API-DOWN-RAN");
    f.run_ok(&["new", "api", "feat/idem"]);

    f.run_ok(&["down", "feat/idem"]);
    let out = f.run_ok(&["down", "feat/idem"]);
    assert!(stderr(&out).contains("API-DOWN-RAN"));
}
