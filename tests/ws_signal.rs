mod common;

use common::{stderr, WorkspaceFixture};

// The interrupting hooks must be declared inline (`add_script`), never as script
// files: `sh -c` forks around a lone script path, so `$PPID` there would be the
// intermediate shell rather than `ws`. The trailing sleep gives the ctrlc handler
// time to flip the flag before the hook returns.

#[test]
fn new_stops_at_repo_boundary_on_sigint() {
    let f = WorkspaceFixture::with_repos(&["api", "app"]);
    f.add_script("api", "post-create", "kill -INT $PPID; sleep 0.4");

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
    f.add_script("api", "up", "kill -INT $PPID; sleep 0.4; echo API-UP-RAN");
    f.add_hook("app", "up", "echo APP-UP-RAN");
    f.run_ok(&["new", "api,app", "feat/sig"]);

    let out = f.run(&["up", "feat/sig"]);
    assert_eq!(out.status.code(), Some(130), "expected exit 130 on SIGINT:\n{}", stderr(&out));

    let err = stderr(&out);
    assert!(err.contains("API-UP-RAN"), "api hook should have completed:\n{err}");
    assert!(!err.contains("APP-UP-RAN"), "app hook must not run after an interrupt:\n{err}");
}
