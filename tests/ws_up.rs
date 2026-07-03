mod common;

use common::{stderr, WorkspaceFixture};

#[test]
fn happy_path_both_hooks_run() {
    let f = WorkspaceFixture::with_repos(&["api", "app"]);
    f.add_hook("api", "up", "echo API-UP-RAN");
    f.add_hook("app", "up", "echo APP-UP-RAN");
    f.run_ok(&["new", "api,app", "feat/foo"]);

    let out = f.run_ok(&["up", "feat/foo"]);
    let err = stderr(&out);

    assert!(err.contains("API-UP-RAN"), "expected api hook output in:\n{err}");
    assert!(err.contains("APP-UP-RAN"), "expected app hook output in:\n{err}");
    // Both step lines resolved ok (non-tty renders "ok").
    assert_eq!(err.matches("up hook").count(), 2, "expected two up-hook steps in:\n{err}");
}

#[test]
fn partial_only_repos_with_worktrees_run() {
    let f = WorkspaceFixture::with_repos(&["api", "app"]);
    f.add_hook("api", "up", "echo API-UP-RAN");
    f.add_hook("app", "up", "echo APP-UP-RAN");
    // Only api gets a worktree for this branch.
    f.run_ok(&["new", "api", "feat/partial"]);

    let out = f.run_ok(&["up", "feat/partial"]);
    let err = stderr(&out);

    assert!(err.contains("API-UP-RAN"), "expected api hook output in:\n{err}");
    assert!(!err.contains("APP-UP-RAN"), "app has no worktree; its hook must not run:\n{err}");
}

#[test]
fn no_worktrees_is_preflight_error() {
    let f = WorkspaceFixture::with_repos(&["api"]);

    let out = f.run(&["up", "feat/none"]);
    assert_eq!(out.status.code(), Some(2), "expected exit 2 when no worktrees");
    let err = stderr(&out);
    assert!(err.contains("no worktrees"), "expected 'no worktrees' in:\n{err}");
}

#[test]
fn missing_hook_is_skipped_not_an_error() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    // No up hook added — repo has a worktree but nothing to run.
    f.run_ok(&["new", "api", "feat/nohook"]);

    let out = f.run_ok(&["up", "feat/nohook"]);
    let err = stderr(&out);
    assert!(err.contains("no up hook"), "expected dim skip line in:\n{err}");
}

#[test]
fn failure_isolation_second_repo_hook_fails() {
    let f = WorkspaceFixture::with_repos(&["api", "app"]);
    f.add_hook("api", "up", "echo API-UP-RAN");
    f.add_hook("app", "up", "echo APP-UP-TRIED; exit 1");
    f.run_ok(&["new", "api,app", "feat/iso"]);

    let out = f.run(&["up", "feat/iso"]);
    assert_eq!(out.status.code(), Some(1), "expected exit 1 when a hook fails");
    let err = stderr(&out);

    // First repo still ran to completion despite the second failing.
    assert!(err.contains("API-UP-RAN"), "first repo should still run:\n{err}");
    assert!(err.contains("APP-UP-TRIED"), "second repo hook should have been attempted:\n{err}");
    assert!(err.contains("FAIL"), "expected a failed step line in:\n{err}");
    assert!(err.contains("app"), "failure summary should name the repo in:\n{err}");
}

#[test]
fn idempotent_second_run_also_succeeds() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.add_hook("api", "up", "echo API-UP-RAN");
    f.run_ok(&["new", "api", "feat/idem"]);

    f.run_ok(&["up", "feat/idem"]);
    // Second run must not fail — nothing ws adds should break hook idempotency.
    let out = f.run_ok(&["up", "feat/idem"]);
    assert!(stderr(&out).contains("API-UP-RAN"));
}

#[test]
fn hook_streaming_stdout_and_stderr() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.add_hook(
        "api",
        "up",
        "echo out1; echo out2; echo err1 >&2; echo out3",
    );
    f.run_ok(&["new", "api", "feat/stream"]);

    let out = f.run_ok(&["up", "feat/stream"]);
    let err = stderr(&out);

    for marker in ["out1", "out2", "out3", "err1"] {
        assert!(err.contains(marker), "expected {marker} in streamed output:\n{err}");
    }
    // Hook lines carry the `<repo>-<event>` tag.
    assert!(err.contains("api-up"), "expected hook tag in:\n{err}");
}
