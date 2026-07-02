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

#[test]
fn stubbed_ops_return_not_implemented() {
    let ws = WorkspaceFixture::empty();
    for args in [
        vec!["new", "foo", "bar/baz"],
        vec!["up", "bar/baz"],
        vec!["down", "bar/baz"],
        vec!["rm", "bar/baz"],
        vec!["list"],
        vec!["here"],
        vec!["use", "main"],
        vec!["clean"],
        vec!["test", "bar/baz"],
    ] {
        let out = ws.run(&args);
        assert!(!out.status.success(), "expected {args:?} to fail (stub)");
        assert!(
            stderr(&out).contains("not yet implemented"),
            "expected 'not yet implemented' for {args:?}, got: {}",
            stderr(&out)
        );
    }
}
