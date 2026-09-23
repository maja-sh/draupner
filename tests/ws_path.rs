mod common;

use common::{stderr, stdout, WorkspaceFixture};

#[test]
fn prints_task_dir_when_only_branch_given() {
    let f = WorkspaceFixture::with_repos(&["api", "app"]);
    f.run_ok(&["new", "api,app", "feat/x"]);

    let out = f.run_ok(&["path", "feat/x"]);
    // ws prints paths through the workspace's canonical root — on macOS
    // `TMPDIR` is a symlink into `/private/var/…`, so f.path() (the raw
    // tempdir) differs from what getcwd resolves to inside the child.
    let expected = f.path().canonicalize().unwrap().join(".worktrees/feat-x");
    assert_eq!(stdout(&out).trim_end(), expected.display().to_string());
}

#[test]
fn prints_repo_worktree_when_repo_given() {
    let f = WorkspaceFixture::with_repos(&["api", "app"]);
    f.run_ok(&["new", "api,app", "feat/x"]);

    let out = f.run_ok(&["path", "feat/x", "-r", "api"]);
    let expected = f.path().canonicalize().unwrap().join(".worktrees/feat-x/api");
    assert_eq!(stdout(&out).trim_end(), expected.display().to_string());
}

#[test]
fn success_stdout_is_only_the_path() {
    // The whole point of the command is `$EDITOR $(ws path …)` — anything
    // on stderr or extra stdout would break the substitution silently.
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/x"]);

    let out = f.run_ok(&["path", "feat/x", "-r", "api"]);
    assert!(stderr(&out).is_empty(), "stderr must be empty on success:\n{}", stderr(&out));
    let s = stdout(&out);
    assert_eq!(s.lines().count(), 1, "stdout must be exactly one line, got:\n{s}");
}

#[test]
fn refuses_when_task_missing() {
    let f = WorkspaceFixture::with_repos(&["api"]);

    let out = f.run(&["path", "feat/nope"]);
    assert_eq!(out.status.code(), Some(2), "missing task is a preflight failure");
    assert!(
        stderr(&out).contains("no worktrees for"),
        "expected 'no worktrees for' in:\n{}",
        stderr(&out)
    );
    assert!(stdout(&out).is_empty(), "no path on stdout when the task is missing");
}

#[test]
fn refuses_unknown_repo() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/x"]);

    let out = f.run(&["path", "feat/x", "-r", "bogus"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("bogus"), "expected 'bogus' in error:\n{}", stderr(&out));
    assert!(stdout(&out).is_empty());
}

#[test]
fn refuses_a_repo_the_task_does_not_include() {
    // `app` is a known repo, but this task was created with only `api`.
    let f = WorkspaceFixture::with_repos(&["api", "app"]);
    f.run_ok(&["new", "api", "feat/x"]);

    let out = f.run(&["path", "feat/x", "-r", "app"]);
    assert_eq!(out.status.code(), Some(2));
    let err = stderr(&out);
    assert!(err.contains("app"), "expected 'app' in diagnostic:\n{err}");
    assert!(err.contains("no worktree"), "expected 'no worktree' in diagnostic:\n{err}");
}

#[test]
fn accepts_sanitized_or_raw_branch_spelling() {
    // `feat/x` and `feat-x` both resolve to `.worktrees/feat-x` — same disk
    // layout, so either spelling on the CLI must land on the same path.
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/x"]);

    let a = f.run_ok(&["path", "feat/x", "-r", "api"]);
    let b = f.run_ok(&["path", "feat-x", "-r", "api"]);
    assert_eq!(stdout(&a), stdout(&b));
}
