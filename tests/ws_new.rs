mod common;

use common::{stderr, WorkspaceFixture};

#[test]
fn new_on_a_remote_workspace_creates_a_sync_session() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(
        r#"{"repos":["api"],"remote":{"connection":"srv","workspace_path":"workspace"}}"#,
    );
    let mg = f.arg_recorder("fake-mutagen");
    let args = f.path().join("mutagen-args.txt");
    // A remote workspace resolves the server `$HOME` up front (for server-side
    // paths); stub ssh to answer that probe so the op doesn't dial a real host.
    let ssh = f.stub("fake-ssh", "printf /home/test");

    let out = f.run_with_env(
        &["new", "api", "feat/foo"],
        &[
            ("WS_MUTAGEN", mg.to_str().unwrap()),
            ("ARGS_FILE", args.to_str().unwrap()),
            ("WS_SSH", ssh.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "ws new failed:\n{}", stderr(&out));

    let rec = std::fs::read_to_string(&args).expect("mutagen was invoked");
    // sync create --name ws-feat-foo-api --ignore-vcs <local> srv:workspace/feat-foo/api
    assert!(rec.contains("create"), "expected `sync create`:\n{rec}");
    assert!(rec.contains("ws-feat-foo-api"), "expected the session name:\n{rec}");
    assert!(rec.contains("--ignore-vcs"), "expected --ignore-vcs:\n{rec}");
    // Mutagen defaults to 0600/0700, which is owner-only — and the owner maps to
    // root in a rootless container, so a service running as www-data can't read
    // the code. Without these the synced tree is unservable.
    assert!(
        rec.contains("--default-file-mode=0644") && rec.contains("--default-directory-mode=0755"),
        "synced tree must be readable by a container's non-root service user:\n{rec}"
    );
    assert!(
        rec.contains("srv:workspace/feat-foo/api"),
        "expected the scp-style beta endpoint:\n{rec}"
    );
}

#[test]
fn new_without_a_remote_block_does_not_sync() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    let mg = f.arg_recorder("fake-mutagen");
    let args = f.path().join("mutagen-args.txt");

    let out = f.run_with_env(
        &["new", "api", "feat/foo"],
        &[("WS_MUTAGEN", mg.to_str().unwrap()), ("ARGS_FILE", args.to_str().unwrap())],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!args.exists(), "no mutagen calls without a remote block");
}

#[test]
fn a_tag_named_main_does_not_shadow_the_default_branch() {
    // Regression: default-branch resolution must probe fully-qualified refspecs
    // so a *tag* called `main` can't be picked as the start-point. Here the repo
    // has no `main`/`master` branch and no origin, only a `main` tag — resolution
    // must fail rather than branch new work off the tag.
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.git("api", &["branch", "-m", "trunk"]); // rename main -> trunk (no main branch)
    f.git("api", &["tag", "main"]); // a tag that short-name rev-parse would resolve

    let out = f.run(&["new", "api", "feat/x"]);
    assert!(!out.status.success(), "should refuse when only a tag matches");
    assert!(
        stderr(&out).contains("default branch"),
        "expected a default-branch error, got:\n{}",
        stderr(&out)
    );
    assert!(!f.path().join(".worktrees/feat-x/api").exists());
}

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
fn worktree_is_clean_despite_symlinked_node_modules() {
    // A repo that gitignores node_modules with the conventional trailing-slash
    // (dir-only) rule. The symlink ws creates is a file to git, so without a
    // worktree exclude it would read as untracked → dirty forever.
    let f = WorkspaceFixture::with_repos(&["app"]);
    std::fs::write(f.path().join("app/.gitignore"), "node_modules/\nvendor/\n").unwrap();
    f.git("app", &["add", ".gitignore"]);
    f.git("app", &["commit", "-q", "-m", "add gitignore"]);
    std::fs::create_dir(f.path().join("app/node_modules")).unwrap();
    std::fs::write(f.path().join("app/node_modules/.installed"), "{}").unwrap();

    f.run_ok(&["new", "app", "feat/clean"]);

    // git status in the worktree must be empty.
    let out = std::process::Command::new("git")
        .args(["-C"])
        .arg(f.path().join(".worktrees/feat-clean/app"))
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    let status = String::from_utf8_lossy(&out.stdout);
    assert!(
        status.trim().is_empty(),
        "worktree should be clean, got:\n{status}"
    );
}

#[test]
fn config_worktree_excludes_are_applied() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(
        r#"{"repos":[{"name":"api","worktree_excludes":["/local.env",".cache/"]}]}"#,
    );

    f.run_ok(&["new", "api", "feat/excl"]);

    // Drop files matching the configured excludes; worktree stays clean.
    std::fs::write(f.path().join(".worktrees/feat-excl/api/local.env"), "X=1").unwrap();
    std::fs::create_dir(f.path().join(".worktrees/feat-excl/api/.cache")).unwrap();
    std::fs::write(f.path().join(".worktrees/feat-excl/api/.cache/blob"), "z").unwrap();

    let out = std::process::Command::new("git")
        .args(["-C"])
        .arg(f.path().join(".worktrees/feat-excl/api"))
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    let status = String::from_utf8_lossy(&out.stdout);
    assert!(status.trim().is_empty(), "expected clean worktree, got:\n{status}");
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

/// Wait briefly for a detached host process to write its marker (`supervise::up`
/// returns as soon as it's launched, so the write races the assertion).
fn wait_for(p: &std::path::Path) -> bool {
    for _ in 0..40 {
        if p.exists() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

fn up_marker_fixture() -> WorkspaceFixture {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    // A host repo — its `up` is launched by ws itself, so this needs no runtime.
    // It drops a marker and then *stays up*: a host `up` is a long-running dev
    // server, and ws now fails one that exits on the spot (which is what a bare
    // `touch` did — passing only because nothing checked the process survived).
    f.write_ws_json(
        r#"{"repos":[{"name":"api","kind":"host","services":{"web":{"port":9000}},
            "scripts":{"up":"touch up-ran && exec sleep 5"}}]}"#,
    );
    f
}

#[test]
fn new_with_up_brings_the_stack_up_in_one_command() {
    // `--up` chains into the same op `ws up` runs, so the worktrees are created
    // AND the stack comes up — no second command, and no `ws up` nudge.
    let f = up_marker_fixture();

    let out = f.run_ok(&["new", "api", "feat/foo", "--up"]);
    assert!(out.status.success(), "ws new --up failed:\n{}", stderr(&out));

    let err = stderr(&out);
    assert!(err.contains("ws-new"), "should still do the create:\n{err}");
    assert!(err.contains("ws-up"), "should chain into up:\n{err}");
    assert!(
        wait_for(&f.path().join(".worktrees/feat-foo/api/up-ran")),
        "up script should have run:\n{err}"
    );
    // The "next: ws up" nudge is pointless once we've already brought it up.
    assert!(!err.contains("next: ws up"), "nudge should be suppressed:\n{err}");

    let _ = f.run(&["rm", "feat/foo", "--force"]);
}

#[test]
fn new_without_up_only_creates_and_nudges() {
    let f = up_marker_fixture();

    let out = f.run_ok(&["new", "api", "feat/foo"]);
    let err = stderr(&out);
    assert!(
        !f.path().join(".worktrees/feat-foo/api/up-ran").exists(),
        "up must not run without --up:\n{err}"
    );
    assert!(err.contains("ws up feat/foo"), "should nudge:\n{err}");
}
