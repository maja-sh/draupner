mod common;

use common::{stderr, WorkspaceFixture};

#[test]
fn happy_path_two_repos() {
    let f = WorkspaceFixture::with_repos(&["api", "app"]);
    f.add_hook("api", "post-create", "echo \"api-hook: wt=$1\"");

    let out = f.run_ok(&["new", "api,app", "feat/login"]);
    let err = stderr(&out);

    // Worktrees created
    assert!(f.path().join(".worktrees/feat-login/api").is_dir());
    assert!(f.path().join(".worktrees/feat-login/app").is_dir());

    // Worktree is a git repo (has .git file)
    assert!(f.path().join(".worktrees/feat-login/api/.git").exists());
    assert!(f.path().join(".worktrees/feat-login/app/.git").exists());

    // Output mentions worktree creation and the hook
    assert!(err.contains("Worktree created"), "expected 'Worktree created' in:\n{err}");
    assert!(err.contains("Post-create hook"), "expected 'Post-create hook' in:\n{err}");
    assert!(err.contains("api-hook: wt="), "expected hook output in:\n{err}");

    // Summary lists worktree paths
    assert!(err.contains("api:"), "expected 'api:' in summary:\n{err}");
    assert!(err.contains("app:"), "expected 'app:' in summary:\n{err}");

    // Nudge
    assert!(err.contains("ws up feat/login"), "expected nudge in:\n{err}");
}

#[test]
fn error_unknown_repo() {
    let f = WorkspaceFixture::with_repos(&["api"]);

    let out = f.run(&["new", "api,bogus", "feat/x"]);
    assert_eq!(out.status.code(), Some(2), "expected exit 2 for unknown repo");

    let err = stderr(&out);
    assert!(err.contains("bogus"), "expected unknown repo name in error:\n{err}");
    assert!(err.contains("api"), "expected known repo listed in error:\n{err}");
}

#[test]
fn error_existing_worktree() {
    let f = WorkspaceFixture::with_repos(&["api"]);

    // First run succeeds
    f.run_ok(&["new", "api", "feat/dup"]);

    // Second run fails
    let out = f.run(&["new", "api", "feat/dup"]);
    assert_eq!(out.status.code(), Some(1), "expected exit 1 for existing worktree");

    let err = stderr(&out);
    assert!(err.contains("ws rm"), "expected 'ws rm' suggestion in error:\n{err}");
}

#[test]
fn hook_exits_nonzero_leaves_worktree() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.add_hook("api", "post-create", "echo 'failing'; exit 1");

    let out = f.run(&["new", "api", "feat/broke"]);
    assert!(!out.status.success(), "expected failure when hook exits non-zero");

    let err = stderr(&out);
    assert!(err.contains("exited 1"), "expected exit code in error:\n{err}");

    // Worktree is left in place per spec
    assert!(
        f.path().join(".worktrees/feat-broke/api").is_dir(),
        "worktree should be left in place after hook failure"
    );
}

#[test]
fn no_hook_still_succeeds() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    // No hook added — post-create template exists but is NOT executable

    let out = f.run_ok(&["new", "api", "feat/nohook"]);
    let err = stderr(&out);

    assert!(f.path().join(".worktrees/feat-nohook/api").is_dir());
    assert!(err.contains("Worktree created"));
    assert!(!err.contains("Post-create hook"), "should not mention hook when none exists");
}

#[test]
fn hook_streaming_stdout_and_stderr() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.add_hook(
        "api",
        "post-create",
        r#"echo "out-line-1"
echo "out-line-2"
echo "out-line-3"
echo "err-line-1" >&2
echo "err-line-2" >&2"#,
    );

    let out = f.run_ok(&["new", "api", "feat/stream"]);
    let err = stderr(&out);

    for needle in &["out-line-1", "out-line-2", "out-line-3", "err-line-1", "err-line-2"] {
        assert!(err.contains(needle), "expected '{needle}' in output:\n{err}");
    }
    // Tag format: [api-post-create]
    assert!(
        err.contains("[api-post-create]"),
        "expected hook tag in output:\n{err}"
    );
}

#[test]
fn forward_compat_unknown_fields_in_ws_json() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(r#"{"repos": ["api"], "futureField": true, "nested": {"x": 1}}"#);

    let out = f.run_ok(&["new", "api", "feat/compat"]);
    assert!(f.path().join(".worktrees/feat-compat/api").is_dir());
    let err = stderr(&out);
    assert!(err.contains("Worktree created"));
}

#[test]
fn branch_sanitization() {
    let f = WorkspaceFixture::with_repos(&["api"]);

    f.run_ok(&["new", "api", "bug/nested/thing"]);

    // Sanitized path uses dashes
    assert!(f.path().join(".worktrees/bug-nested-thing/api").is_dir());
}

#[test]
fn deduplicates_repo_list() {
    let f = WorkspaceFixture::with_repos(&["api"]);

    let out = f.run_ok(&["new", "api,api", "feat/dedup"]);
    let err = stderr(&out);

    // Should only create one worktree, not fail with "already exists"
    assert!(f.path().join(".worktrees/feat-dedup/api").is_dir());
    // "Worktree created" should appear exactly once
    let count = err.matches("Worktree created").count();
    assert_eq!(count, 1, "expected exactly 1 'Worktree created', got {count}");
}

#[test]
fn node_modules_symlinked_when_present() {
    let f = WorkspaceFixture::with_repos(&["app"]);

    // Create node_modules in the main checkout
    std::fs::create_dir(f.path().join("app/node_modules")).expect("mkdir node_modules");
    std::fs::write(f.path().join("app/node_modules/.package-lock.json"), "{}").unwrap();

    let out = f.run_ok(&["new", "app", "feat/nm"]);
    let err = stderr(&out);

    let link = f.path().join(".worktrees/feat-nm/app/node_modules");
    assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    assert!(err.contains("node_modules symlinked"));
}

#[test]
fn vendor_copied_when_composer_present() {
    let f = WorkspaceFixture::with_repos(&["api"]);

    // Create composer.json + vendor/ in the main checkout
    std::fs::write(f.path().join("api/composer.json"), "{}").unwrap();
    let vendor = f.path().join("api/vendor");
    std::fs::create_dir(&vendor).unwrap();
    std::fs::write(vendor.join("autoload.php"), "<?php // autoload").unwrap();

    let out = f.run_ok(&["new", "api", "feat/vendor"]);
    let err = stderr(&out);

    let copied = f.path().join(".worktrees/feat-vendor/api/vendor/autoload.php");
    assert!(copied.is_file(), "vendor/autoload.php should be copied");
    assert!(err.contains("vendor/ copied"));
}

#[test]
fn ws_env_vars_passed_to_hook() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.add_hook(
        "api",
        "post-create",
        r#"echo "WS_REPO=$WS_REPO"
echo "WS_BRANCH_SANITIZED=$WS_BRANCH_SANITIZED"
echo "WS_HOOK_NAME=$WS_HOOK_NAME"
echo "WS_WORKSPACE_ROOT=$WS_WORKSPACE_ROOT""#,
    );

    let out = f.run_ok(&["new", "api", "feat/env"]);
    let err = stderr(&out);

    assert!(err.contains("WS_REPO=api"), "expected WS_REPO=api in:\n{err}");
    assert!(
        err.contains("WS_BRANCH_SANITIZED=feat-env"),
        "expected WS_BRANCH_SANITIZED=feat-env in:\n{err}"
    );
    assert!(
        err.contains("WS_HOOK_NAME=post-create"),
        "expected WS_HOOK_NAME=post-create in:\n{err}"
    );
    assert!(
        err.contains("WS_WORKSPACE_ROOT="),
        "expected WS_WORKSPACE_ROOT in:\n{err}"
    );
}
