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
