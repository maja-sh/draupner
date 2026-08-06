mod common;

use common::{stderr, WorkspaceFixture};

// Hooks send SIGINT to ws via $PPID and sleep so the handler flips its flag before the hook returns, so ws stops at the next repo boundary and exits 130.

#[test]
fn new_stops_at_repo_boundary_on_sigint() {
    let f = WorkspaceFixture::with_repos(&["api", "app"]);
    // The sleep gives the ctrlc handler time to set the flag before this repo's iteration finishes.
    f.add_hook("api", "post-create", "kill -INT $PPID; sleep 0.4");

    let out = f.run(&["new", "api,app", "feat/sig"]);
    assert_eq!(out.status.code(), Some(130), "expected exit 130 on SIGINT:\n{}", stderr(&out));

    assert!(f.path().join(".worktrees/feat-sig/api").is_dir(), "api worktree should exist");
    assert!(
        !f.path().join(".worktrees/feat-sig/app").exists(),
        "app must not be processed after an interrupt"
    );
}

#[test]
fn up_sweep_stops_on_sigint() {
    let f = WorkspaceFixture::with_repos(&["api", "app"]);
    f.add_hook("api", "up", "kill -INT $PPID; sleep 0.4; echo API-UP-RAN");
    f.add_hook("app", "up", "echo APP-UP-RAN");
    f.run_ok(&["new", "api,app", "feat/sig"]);

    let out = f.run(&["up", "feat/sig"]);
    assert_eq!(out.status.code(), Some(130), "expected exit 130 on SIGINT:\n{}", stderr(&out));

    let err = stderr(&out);
    assert!(err.contains("API-UP-RAN"), "api hook should have completed:\n{err}");
    assert!(!err.contains("APP-UP-RAN"), "app hook must not run after an interrupt:\n{err}");
}
