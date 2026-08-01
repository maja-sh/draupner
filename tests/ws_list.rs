mod common;

use common::{stderr, WorkspaceFixture};

#[test]
fn empty_workspace_reports_no_worktrees() {
    let f = WorkspaceFixture::with_repos(&["api"]);

    let out = f.run_ok(&["list"]);
    let err = stderr(&out);
    assert!(err.contains("no worktrees"), "expected 'no worktrees' in:\n{err}");
}

#[test]
fn one_task_one_repo_shows_table() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/foo"]);

    let out = f.run_ok(&["list"]);
    let err = stderr(&out);

    // Group label is the (unsanitized) branch name.
    assert!(err.contains("feat/foo"), "expected branch label in:\n{err}");
    assert!(err.contains("api"), "expected repo in:\n{err}");
    assert!(err.contains("clean"), "expected clean status in:\n{err}");
}

#[test]
fn multiple_tasks_are_grouped() {
    let f = WorkspaceFixture::with_repos(&["api", "app"]);
    f.run_ok(&["new", "api,app", "feat/one"]);
    f.run_ok(&["new", "api", "bug/two"]);

    let out = f.run_ok(&["list"]);
    let err = stderr(&out);

    assert!(err.contains("feat/one"), "expected feat/one group in:\n{err}");
    assert!(err.contains("bug/two"), "expected bug/two group in:\n{err}");
}

#[test]
fn dirty_worktree_shows_dirty() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/dirty"]);

    // Introduce an untracked file in the worktree.
    std::fs::write(
        f.path().join(".worktrees/feat-dirty/api/scratch.txt"),
        "wip",
    )
    .unwrap();

    let out = f.run_ok(&["list"]);
    let err = stderr(&out);
    assert!(err.contains("dirty"), "expected 'dirty' in:\n{err}");
}

#[test]
fn missing_status_hook_does_not_crash() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/nostatus"]);

    // No status hook present — op should still succeed and list the worktree.
    let out = f.run_ok(&["list"]);
    let err = stderr(&out);
    assert!(err.contains("feat/nostatus"), "expected worktree listed in:\n{err}");

    // With no status hook anywhere, the status column is suppressed entirely —
    // no bare placeholder dash dangling off the row.
    let row = err
        .lines()
        .find(|l| l.contains("api") && l.contains("clean"))
        .expect("expected an 'api … clean' row");
    assert!(
        !row.contains('-'),
        "status dash should be suppressed when nothing reports status: {row:?}"
    );
}

#[test]
fn status_script_naming_ws_hooks_path_is_anchored_to_workspace_root() {
    // A declared `status` script runs with cwd = the worktree, but `.ws-hooks/`
    // lives at the workspace root. Regression: `capture_command` used to join the
    // script's commands without anchoring, so this exited 127 and `ws list`
    // showed an empty status column — while the same value worked for `up`
    // (which anchors) and on a remote workspace.
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    let hook = f.path().join(".ws-hooks").join("api-status.sh");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(&hook, "#!/bin/sh\necho 'stack up   https://anchored:8441'\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    f.write_ws_json(
        r#"{"repos":[{"name":"api","scripts":{"status":".ws-hooks/api-status.sh"}}]}"#,
    );
    f.run_ok(&["new", "api", "feat/anchored"]);

    let out = f.run_ok(&["list"]);
    let err = stderr(&out);
    assert!(
        err.contains("stack up") && err.contains("https://anchored:8441"),
        "expected the `.ws-hooks/`-relative status script's output in:\n{err}"
    );
}

#[test]
fn status_hook_output_is_shown() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.add_hook("api", "status", "echo 'stack up   https://example:8440'");
    f.run_ok(&["new", "api", "feat/withstatus"]);

    let out = f.run_ok(&["list"]);
    let err = stderr(&out);
    assert!(
        err.contains("stack up") && err.contains("https://example:8440"),
        "expected status hook output in:\n{err}"
    );
}
