mod common;

use common::{stderr, WorkspaceFixture};

#[test]
fn lists_sibling_worktrees_from_inside_one() {
    let f = WorkspaceFixture::with_repos(&["api", "app"]);
    f.run_ok(&["new", "api,app", "feat/foo"]);

    // Run from inside the api worktree — `ws here` should surface both siblings.
    let out = f.run_in(".worktrees/feat-foo/api", &["here"]);
    assert!(out.status.success(), "ws here should succeed from inside a worktree");
    let err = stderr(&out);

    assert!(err.contains("api:"), "expected api sibling path in:\n{err}");
    assert!(err.contains("app:"), "expected app sibling path in:\n{err}");
    assert!(
        err.contains(".worktrees/feat-foo/app"),
        "expected the app worktree path in:\n{err}"
    );
}

#[test]
fn lists_only_existing_siblings() {
    let f = WorkspaceFixture::with_repos(&["api", "app"]);
    // Only api has a worktree for this task.
    f.run_ok(&["new", "api", "feat/partial"]);

    let out = f.run_in(".worktrees/feat-partial/api", &["here"]);
    assert!(out.status.success());
    let err = stderr(&out);

    assert!(err.contains("api:"), "expected api sibling in:\n{err}");
    assert!(!err.contains("app:"), "app has no worktree; must not be listed:\n{err}");
}

#[test]
fn not_inside_worktree_is_preflight_error() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/foo"]);

    // From the workspace root — not inside any task worktree.
    let out = f.run(&["here"]);
    assert_eq!(out.status.code(), Some(2), "expected exit 2 outside a worktree");
    assert!(stderr(&out).contains("not inside a task worktree"));
}

#[test]
fn detection_survives_a_symlinked_cwd() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/foo"]);

    // A symlink pointing at the api worktree. Running `ws here` "through" it must
    // still resolve to the real task via the canonical path.
    #[cfg(unix)]
    {
        let link = f.path().join("shortcut");
        std::os::unix::fs::symlink(f.path().join(".worktrees/feat-foo/api"), &link).unwrap();

        let out = f.run_in("shortcut", &["here"]);
        assert!(out.status.success(), "ws here should resolve through a symlink:\n{}", stderr(&out));
        assert!(stderr(&out).contains("api:"));
    }
}
