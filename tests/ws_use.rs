mod common;

use common::{stderr, WorkspaceFixture};
use std::process::Command;

/// Read a repo's current branch directly (test-side oracle).
fn head_branch(repo: &std::path::Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn no_arg_prints_current_branches() {
    let f = WorkspaceFixture::with_repos(&["api", "app"]);

    let out = f.run_ok(&["use"]);
    let err = stderr(&out);

    // Both repos start on `main`.
    assert!(err.contains("api: main"), "expected api branch in:\n{err}");
    assert!(err.contains("app: main"), "expected app branch in:\n{err}");
}

#[test]
fn with_arg_switches_branch() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    // Create a branch (not checked out anywhere but main's history).
    f.git("api", &["branch", "feature/x"]);

    f.run_ok(&["use", "feature/x"]);

    assert_eq!(head_branch(&f.path().join("api")), "feature/x");
}

#[test]
fn branch_owned_by_worktree_is_detached_first() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    // `ws new` checks out feat/owned in a linked worktree.
    f.run_ok(&["new", "api", "feat/owned"]);
    let wt = f.path().join(".worktrees/feat-owned/api");

    // `ws use` should detach the worktree and move the branch to main.
    f.run_ok(&["use", "feat/owned"]);

    assert_eq!(head_branch(&f.path().join("api")), "feat/owned", "main should own the branch now");
    // The worktree is left in detached HEAD.
    let wt_head = Command::new("git")
        .arg("-C")
        .arg(&wt)
        .args(["symbolic-ref", "--quiet", "HEAD"])
        .status()
        .unwrap();
    assert!(!wt_head.success(), "worktree should be in detached HEAD after use");
}

#[test]
fn unknown_branch_is_preflight_error() {
    let f = WorkspaceFixture::with_repos(&["api"]);

    let out = f.run(&["use", "does/not-exist"]);
    assert_eq!(out.status.code(), Some(2), "expected exit 2 for unknown branch");
    assert!(stderr(&out).contains("not found"), "expected not-found message");
}
