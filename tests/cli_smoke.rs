mod common;

use common::{stderr, stdout, WorkspaceFixture};

#[test]
fn version_flag_prints_version() {
    let ws = WorkspaceFixture::empty();
    let out = ws.run_ok(&["--version"]);
    assert!(stdout(&out).contains("ws 0.1.0"));
}

#[test]
fn no_args_prints_usage_and_exits_nonzero() {
    let ws = WorkspaceFixture::empty();
    let out = ws.run(&[]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("Usage:") || err.contains("usage"));
}
