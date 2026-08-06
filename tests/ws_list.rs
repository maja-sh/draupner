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
fn recorded_host_ports_are_shown_as_sub_rows() {
    // The port a repo actually publishes was invisible in `ws list` — a
    // container repo's status hook summarizes (`6 up`), and the sibling DB you
    // need to dial has no representation at all. Recorded ports live in
    // `.ws-meta`; the row now surfaces them.
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/ports"]);

    // Simulate what `ws up` records: two allocated services under this repo.
    let meta_dir = f.path().join(".ws-meta");
    std::fs::create_dir_all(&meta_dir).unwrap();
    std::fs::write(
        meta_dir.join("feat-ports-api.json"),
        r#"{"ports":{"mysql":20021,"redis":20022}}"#,
    )
    .unwrap();

    let out = f.run_ok(&["list"]);
    let err = stderr(&out);
    assert!(
        err.contains("mysql") && err.contains(":20021"),
        "expected mysql :20021 sub-row in:\n{err}"
    );
    assert!(
        err.contains("redis") && err.contains(":20022"),
        "expected redis :20022 sub-row in:\n{err}"
    );
}

#[test]
fn a_repo_with_no_recorded_ports_emits_no_sub_rows() {
    // The compact case: a worktree that predates `up` (or a host repo whose
    // sole port is already spoken for by the status column) must not sprout
    // dead sub-rows or blank ones.
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/empty"]);

    let out = f.run_ok(&["list"]);
    let err = stderr(&out);
    // Every non-empty line after the group header contains either the repo
    // name ("api") or the group label / path — nothing bare-indented at 6
    // spaces (the sub-row indent).
    for line in err.lines() {
        assert!(
            !line.starts_with("      ") || line.trim().is_empty(),
            "unexpected sub-row emitted with nothing to show: {line:?}"
        );
    }
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
