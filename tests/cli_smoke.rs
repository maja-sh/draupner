mod common;

use common::{stderr, stdout, WorkspaceFixture};

#[test]
fn version_flag_prints_version() {
    let ws = WorkspaceFixture::empty();
    let out = ws.run_ok(&["--version"]);
    // Track the package version so a bump never breaks this (it did once, at 0.2.0).
    assert!(stdout(&out).contains(&format!("ws {}", env!("CARGO_PKG_VERSION"))));
}

#[test]
fn no_args_prints_usage_and_exits_nonzero() {
    let ws = WorkspaceFixture::empty();
    let out = ws.run(&[]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("Usage:") || err.contains("usage"));
}
