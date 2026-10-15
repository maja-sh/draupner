mod common;

use common::{stderr, WorkspaceFixture};
use std::path::Path;
use std::process::Command;

/// The `git status --porcelain` command for a worktree, carrying the isolated git
/// env every other git invocation in a fixture runs with.
///
/// A bare `Command::new("git")` inherits the developer's real `$HOME` and an unset
/// `GIT_CONFIG_GLOBAL`, so a global `core.excludesFile` listing `node_modules` (a
/// very common local setup) made the cleanliness tests below pass even with the
/// worktree-exclude write removed — silently voiding the only regression coverage
/// `add_worktree_excludes` has.
fn git_status_cmd(wt: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(wt)
        .args(["status", "--porcelain"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    cmd
}

/// `git status --porcelain` in a worktree. Panics if git itself failed: a git that
/// dies (exit 128 — bad config, no repo) writes nothing to stdout, which an
/// stdout-only check happily reads as "clean".
fn git_status(wt: &Path) -> String {
    let out = git_status_cmd(wt).output().expect("run git status");
    assert!(
        out.status.success(),
        "git status in {} failed (exit {:?}):\n{}",
        wt.display(),
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn new_on_a_remote_workspace_creates_a_sync_session() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(
        r#"{"repos":["api"],"remote":{"connection":"srv","workspace_path":"workspace"}}"#,
    );
    let mg = f.arg_recorder("fake-mutagen");
    let args = f.path().join("mutagen-args.txt");
    // A remote op probes the server for $HOME; stub ssh so it doesn't dial a real host.
    let ssh = f.stub("fake-ssh", "printf /home/test");

    let out = f.run_with_env(
        &["new", "api", "feat/foo"],
        &[
            ("DRUP_MUTAGEN", mg.to_str().unwrap()),
            ("ARGS_FILE", args.to_str().unwrap()),
            ("DRUP_SSH", ssh.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "drup new failed:\n{}", stderr(&out));

    let rec = std::fs::read_to_string(&args).expect("mutagen was invoked");
    assert!(rec.contains("create"), "expected `sync create`:\n{rec}");
    assert!(rec.contains("drup-feat-foo-api"), "expected the session name:\n{rec}");
    assert!(rec.contains("--ignore-vcs"), "expected --ignore-vcs:\n{rec}");
    // Mutagen's 0600/0700 defaults map to root in a rootless container, leaving the tree unreadable to non-root service users.
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
        &[("DRUP_MUTAGEN", mg.to_str().unwrap()), ("ARGS_FILE", args.to_str().unwrap())],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!args.exists(), "no mutagen calls without a remote block");
}

#[test]
fn a_tag_named_main_does_not_shadow_the_default_branch() {
    // Regression: a tag named `main` must not be picked as the start-point when no main/master branch exists.
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.git("api", &["branch", "-m", "trunk"]);
    f.git("api", &["tag", "main"]);

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
fn a_dangling_origin_head_falls_back_to_the_default_branch_ladder() {
    // Regression: upstream renamed its default branch, so a pruning fetch deleted
    // `refs/remotes/origin/master` but left `origin/HEAD` pointing at it.
    // `git symbolic-ref` doesn't dereference, so it keeps reporting the dead
    // branch — taking it as the start point aborted `drup new` with "not a valid
    // object name: 'origin/master'" even though `origin/main` resolves fine.
    let f = WorkspaceFixture::empty();
    f.add_repo_with_origin("api"); // pushes + tracks `main`, so origin/main exists
    f.write_ws_json(r#"{"repos":["api"]}"#);
    f.git(
        "api",
        &["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/master"],
    );

    let out = f.run(&["new", "api", "feat/dangling"]);
    assert!(
        out.status.success(),
        "drup new must fall through to the main/master ladder:\n{}",
        stderr(&out)
    );

    let wt = f.path().join(".worktrees/feat-dangling/api");
    assert!(wt.is_dir(), "worktree should have been created");
    // …and off `origin/main`, not off some other commit.
    let head = rev_parse(&wt, "HEAD");
    let origin_main = rev_parse(&f.path().join("api"), "refs/remotes/origin/main");
    assert_eq!(head, origin_main, "worktree should start from origin/main");
}

/// The commit `rev` resolves to in `repo`, for asserting a worktree's start point.
fn rev_parse(repo: &std::path::Path, rev: &str) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", rev])
        .output()
        .expect("git rev-parse");
    assert!(out.status.success(), "rev-parse {rev} failed in {}", repo.display());
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn happy_path_two_repos() {
    let f = WorkspaceFixture::with_repos(&["api", "app"]);
    f.add_hook("api", "post-create", "echo \"api-hook: wt=$DRUP_WORKTREE\"");

    let out = f.run_ok(&["new", "api,app", "feat/login"]);
    let err = stderr(&out);

    assert!(f.path().join(".worktrees/feat-login/api").is_dir());
    assert!(f.path().join(".worktrees/feat-login/app").is_dir());

    assert!(f.path().join(".worktrees/feat-login/api/.git").exists());
    assert!(f.path().join(".worktrees/feat-login/app/.git").exists());

    assert!(err.contains("Worktree created"), "expected 'Worktree created' in:\n{err}");
    assert!(err.contains("Post-create hook"), "expected 'Post-create hook' in:\n{err}");
    assert!(err.contains("api-hook: wt="), "expected hook output in:\n{err}");

    assert!(err.contains("api:"), "expected 'api:' in summary:\n{err}");
    assert!(err.contains("app:"), "expected 'app:' in summary:\n{err}");

    assert!(err.contains("drup up feat/login"), "expected nudge in:\n{err}");
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

    f.run_ok(&["new", "api", "feat/dup"]);

    let out = f.run(&["new", "api", "feat/dup"]);
    assert_eq!(out.status.code(), Some(1), "expected exit 1 for existing worktree");

    let err = stderr(&out);
    assert!(err.contains("drup rm"), "expected 'drup rm' suggestion in error:\n{err}");
}

#[test]
fn hook_exits_nonzero_rolls_back_the_worktree() {
    // `drup new` is atomic across every fallible step past `create_worktree`: a
    // post-create hook that exits non-zero unwinds the worktree so a rerun
    // isn't blocked by half-created state (previously the worktree stayed and
    // the user had to `drup rm` before retrying).
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.add_hook("api", "post-create", "echo 'failing'; exit 1");

    let out = f.run(&["new", "api", "feat/broke"]);
    assert!(!out.status.success(), "expected failure when hook exits non-zero");

    let err = stderr(&out);
    assert!(err.contains("exited 1"), "expected exit code in error:\n{err}");

    assert!(
        !f.path().join(".worktrees/feat-broke/api").exists(),
        "worktree must be rolled back when the hook fails, not left orphaned"
    );
}

#[test]
fn second_repo_hook_failure_rolls_back_the_first_repo_too() {
    // The scenario the pre-oss review flagged: two repos, second fails, first
    // was fully created (worktree + branch). Without cross-repo rollback the
    // first repo's worktree lingered and the only path forward was `drup rm`
    // (which then wiped the sibling too), so the user paid the same cost for
    // a partial success. Both must unwind so a retry is clean.
    let f = WorkspaceFixture::with_repos(&["api", "web"]);
    f.add_hook("web", "post-create", "echo failing; exit 1");

    let out = f.run(&["new", "api,web", "feat/broke"]);
    assert!(!out.status.success(), "expected failure when repo 2's hook exits non-zero");

    assert!(
        !f.path().join(".worktrees/feat-broke/api").exists(),
        "repo 1 must be rolled back after repo 2 fails, not left orphaned"
    );
    assert!(
        !f.path().join(".worktrees/feat-broke/web").exists(),
        "repo 2's partial worktree must be rolled back"
    );
    assert!(
        !f.path().join(".worktrees/feat-broke").exists(),
        "the empty task dir must be swept once its repos are gone"
    );
}

#[test]
fn no_hook_still_succeeds() {
    let f = WorkspaceFixture::with_repos(&["api"]);

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

    assert!(f.path().join(".worktrees/bug-nested-thing/api").is_dir());
}

#[test]
fn deduplicates_repo_list() {
    let f = WorkspaceFixture::with_repos(&["api"]);

    let out = f.run_ok(&["new", "api,api", "feat/dedup"]);
    let err = stderr(&out);

    assert!(f.path().join(".worktrees/feat-dedup/api").is_dir());
    let count = err.matches("Worktree created").count();
    assert_eq!(count, 1, "expected exactly 1 'Worktree created', got {count}");
}

#[test]
fn node_modules_symlinked_when_present() {
    let f = WorkspaceFixture::with_repos(&["app"]);

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
fn tracked_vendor_is_not_copied_over_itself() {
    // A repo that commits its vendor/: `git worktree add` already checked it out,
    // so copying main's on top would nest it (`cp -R src dst` copies *into* an
    // existing dst) and leave a silent duplicate dependency tree at vendor/vendor.
    let f = WorkspaceFixture::with_repos(&["api"]);

    std::fs::write(f.path().join("api/composer.json"), "{}").unwrap();
    let vendor = f.path().join("api/vendor");
    std::fs::create_dir_all(vendor.join("acme/lib")).unwrap();
    std::fs::write(vendor.join("autoload.php"), "<?php // autoload").unwrap();
    std::fs::write(vendor.join("acme/lib/Lib.php"), "<?php // lib").unwrap();
    f.git("api", &["add", "composer.json", "vendor"]);
    f.git("api", &["commit", "-q", "-m", "commit vendor"]);

    let out = f.run_ok(&["new", "api", "feat/tracked-vendor"]);
    let err = stderr(&out);

    let wt = f.path().join(".worktrees/feat-tracked-vendor/api");
    assert!(
        !wt.join("vendor/vendor").exists(),
        "vendor/ must not be copied into the worktree's own tracked vendor/"
    );
    // The tracked copy git checked out is still there and complete.
    assert!(wt.join("vendor/autoload.php").is_file());
    assert!(wt.join("vendor/acme/lib/Lib.php").is_file());
    assert!(
        !err.contains("vendor/ copied"),
        "no copy happens, so no copy step should be reported:\n{err}"
    );
}

#[test]
fn worktree_is_clean_despite_symlinked_node_modules() {
    // The gitignore's trailing-slash rule is dir-only, so a node_modules symlink would read as untracked without the worktree exclude drup installs.
    let f = WorkspaceFixture::with_repos(&["app"]);
    std::fs::write(f.path().join("app/.gitignore"), "node_modules/\nvendor/\n").unwrap();
    f.git("app", &["add", ".gitignore"]);
    f.git("app", &["commit", "-q", "-m", "add gitignore"]);
    std::fs::create_dir(f.path().join("app/node_modules")).unwrap();
    std::fs::write(f.path().join("app/node_modules/.installed"), "{}").unwrap();

    f.run_ok(&["new", "app", "feat/clean"]);

    let status = git_status(&f.path().join(".worktrees/feat-clean/app"));
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

    std::fs::write(f.path().join(".worktrees/feat-excl/api/local.env"), "X=1").unwrap();
    std::fs::create_dir(f.path().join(".worktrees/feat-excl/api/.cache")).unwrap();
    std::fs::write(f.path().join(".worktrees/feat-excl/api/.cache/blob"), "z").unwrap();

    let status = git_status(&f.path().join(".worktrees/feat-excl/api"));
    assert!(status.trim().is_empty(), "expected clean worktree, got:\n{status}");
}

#[test]
fn git_status_helper_runs_with_the_isolated_git_env() {
    // The cleanliness checks above are the only regression coverage for the
    // worktree-exclude write; reading the developer's own gitconfig (a global
    // `core.excludesFile` with `node_modules` in it) makes them pass regardless.
    let cmd = git_status_cmd(Path::new("."));
    let envs: Vec<(&std::ffi::OsStr, Option<&std::ffi::OsStr>)> = cmd.get_envs().collect();
    for key in ["GIT_CONFIG_GLOBAL", "GIT_CONFIG_SYSTEM"] {
        assert!(
            envs.contains(&(
                std::ffi::OsStr::new(key),
                Some(std::ffi::OsStr::new("/dev/null"))
            )),
            "worktree cleanliness must be judged with {key}=/dev/null, got {envs:?}"
        );
    }
}

#[test]
fn git_status_helper_does_not_read_a_failed_git_as_clean() {
    let f = WorkspaceFixture::empty();
    let broken = f.path().join("broken");
    std::fs::create_dir(&broken).unwrap();
    // A `.git` *file* pointing nowhere: discovery stops here, so git dies with
    // exit 128 and an empty stdout — indistinguishable from a clean worktree to
    // anything that only looks at stdout.
    std::fs::write(broken.join(".git"), "gitdir: /nonexistent/gitdir\n").unwrap();

    let out = git_status_cmd(&broken).output().expect("run git status");
    assert!(!out.status.success(), "precondition: this git status must fail");
    assert!(out.stdout.is_empty(), "precondition: a failing git status prints no status");

    let panicked = std::panic::catch_unwind(|| git_status(&broken)).is_err();
    assert!(panicked, "a `git status` that failed must not be reported as a clean worktree");
}

#[test]
fn drup_env_vars_passed_to_hook() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.add_hook(
        "api",
        "post-create",
        r#"echo "DRUP_REPO=$DRUP_REPO"
echo "DRUP_BRANCH_SANITIZED=$DRUP_BRANCH_SANITIZED"
echo "DRUP_HOOK_NAME=$DRUP_HOOK_NAME"
echo "DRUP_WORKSPACE_ROOT=$DRUP_WORKSPACE_ROOT""#,
    );

    let out = f.run_ok(&["new", "api", "feat/env"]);
    let err = stderr(&out);

    assert!(err.contains("DRUP_REPO=api"), "expected DRUP_REPO=api in:\n{err}");
    assert!(
        err.contains("DRUP_BRANCH_SANITIZED=feat-env"),
        "expected DRUP_BRANCH_SANITIZED=feat-env in:\n{err}"
    );
    assert!(
        err.contains("DRUP_HOOK_NAME=post-create"),
        "expected DRUP_HOOK_NAME=post-create in:\n{err}"
    );
    assert!(
        err.contains("DRUP_WORKSPACE_ROOT="),
        "expected DRUP_WORKSPACE_ROOT in:\n{err}"
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
    // A host `up` must be long-running; a bare `touch` would exit and now fail the survivorship check.
    f.write_ws_json(
        r#"{"repos":[{"name":"api","kind":"host","services":{"web":{"port":9000}},
            "scripts":{"up":"touch up-ran && exec sleep 5"}}]}"#,
    );
    f
}

#[test]
fn new_with_up_brings_the_stack_up_in_one_command() {
    let f = up_marker_fixture();

    let out = f.run_ok(&["new", "api", "feat/foo", "--up"]);
    assert!(out.status.success(), "drup new --up failed:\n{}", stderr(&out));

    let err = stderr(&out);
    assert!(err.contains("drup-new"), "should still do the create:\n{err}");
    assert!(err.contains("drup-up"), "should chain into up:\n{err}");
    assert!(
        wait_for(&f.path().join(".worktrees/feat-foo/api/up-ran")),
        "up script should have run:\n{err}"
    );
    // The "next: drup up" nudge is pointless once we've already brought it up.
    assert!(!err.contains("next: drup up"), "nudge should be suppressed:\n{err}");

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
    assert!(err.contains("drup up feat/foo"), "should nudge:\n{err}");
}
