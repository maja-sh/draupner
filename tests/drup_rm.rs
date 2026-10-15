mod common;

use common::{stderr, WorkspaceFixture};

#[test]
fn rm_on_a_remote_workspace_terminates_the_sync_session() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(
        r#"{"repos":["api"],"remote":{"connection":"srv","workspace_path":"workspace"}}"#,
    );
    let mg = f.arg_recorder("fake-mutagen");
    // Remote workspaces probe the server `$HOME` on new/rm; stub ssh so the op doesn't dial a real host.
    let ssh = f.stub("fake-ssh", "printf /home/test");

    let create_args = f.path().join("create-args.txt");
    f.run_with_env(
        &["new", "api", "feat/foo"],
        &[
            ("DRUP_MUTAGEN", mg.to_str().unwrap()),
            ("ARGS_FILE", create_args.to_str().unwrap()),
            ("DRUP_SSH", ssh.to_str().unwrap()),
        ],
    );

    let term_args = f.path().join("term-args.txt");
    let out = f.run_with_env(
        &["rm", "feat/foo"],
        &[
            ("DRUP_MUTAGEN", mg.to_str().unwrap()),
            ("ARGS_FILE", term_args.to_str().unwrap()),
            ("DRUP_SSH", ssh.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "drup rm failed:\n{}", stderr(&out));

    let rec = std::fs::read_to_string(&term_args).expect("mutagen was invoked on rm");
    assert!(rec.contains("terminate"), "expected `sync terminate`:\n{rec}");
    assert!(rec.contains("drup-feat-foo-api"), "expected the session name:\n{rec}");
}

#[test]
fn happy_path_clean_teardown() {
    let f = WorkspaceFixture::with_repos(&["api", "app"]);
    f.run_ok(&["new", "api,app", "feat/gone"]);
    assert!(f.path().join(".worktrees/feat-gone/api").is_dir());

    let out = f.run_ok(&["rm", "feat/gone"]);
    let err = stderr(&out);

    assert!(!f.path().join(".worktrees/feat-gone/api").exists());
    assert!(!f.path().join(".worktrees/feat-gone/app").exists());
    assert!(!f.path().join(".worktrees/feat-gone").exists());
    assert!(err.contains("Removed worktrees"), "expected summary in:\n{err}");

    let out = f.run_ok(&["list"]);
    assert!(stderr(&out).contains("no worktrees"));
}

#[test]
fn dirty_without_force_blocks() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/dirty"]);

    // Track then modify a file so it shows as a real (non-untracked) change.
    let wt = f.path().join(".worktrees/feat-dirty/api");
    std::fs::write(wt.join("tracked.txt"), "v1").unwrap();
    f.git(".worktrees/feat-dirty/api", &["add", "tracked.txt"]);
    f.git(".worktrees/feat-dirty/api", &["commit", "-q", "-m", "add"]);
    std::fs::write(wt.join("tracked.txt"), "v2").unwrap();

    let out = f.run(&["rm", "feat/dirty"]);
    assert_eq!(out.status.code(), Some(1), "expected exit 1 for dirty worktree");

    let err = stderr(&out);
    assert!(err.contains("uncommitted changes"), "expected dirty warning in:\n{err}");
    assert!(err.contains("tracked.txt"), "expected modified file listed in:\n{err}");
    assert!(err.contains("--force"), "expected --force hint in:\n{err}");

    assert!(wt.is_dir(), "worktree should survive a blocked rm");
}

#[test]
fn dirty_with_force_tears_down() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/forced"]);

    let wt = f.path().join(".worktrees/feat-forced/api");
    std::fs::write(wt.join("tracked.txt"), "v1").unwrap();
    f.git(".worktrees/feat-forced/api", &["add", "tracked.txt"]);
    f.git(".worktrees/feat-forced/api", &["commit", "-q", "-m", "add"]);
    std::fs::write(wt.join("tracked.txt"), "v2").unwrap();

    let out = f.run_ok(&["rm", "feat/forced", "--force"]);
    let err = stderr(&out);

    assert!(err.contains("uncommitted changes"), "expected override warning in:\n{err}");
    assert!(!wt.exists(), "worktree should be removed with --force");
}

#[test]
fn unpushed_without_force_blocks() {
    let f = WorkspaceFixture::empty();
    f.add_repo_with_origin("api");
    f.write_ws_json(r#"{"repos":["api"]}"#);

    // Push a feature branch so a new worktree on it has an upstream.
    f.git("api", &["checkout", "-q", "-b", "feat/up"]);
    f.git("api", &["commit", "-q", "--allow-empty", "-m", "base"]);
    f.git("api", &["push", "-q", "-u", "origin", "feat/up"]);
    f.git("api", &["checkout", "-q", "main"]);

    f.run_ok(&["new", "api", "feat/up"]);

    let wt_rel = ".worktrees/feat-up/api";
    f.git(wt_rel, &["commit", "-q", "--allow-empty", "-m", "local work"]);

    let out = f.run(&["rm", "feat/up"]);
    assert_eq!(out.status.code(), Some(1), "expected exit 1 for unpushed commits");

    let err = stderr(&out);
    assert!(err.contains("unpushed commits"), "expected unpushed warning in:\n{err}");
    assert!(err.contains("local work"), "expected unpushed commit listed in:\n{err}");
}

#[test]
fn no_worktrees_is_preflight_error() {
    let f = WorkspaceFixture::with_repos(&["api"]);

    let out = f.run(&["rm", "feat/nonexistent"]);
    assert_eq!(out.status.code(), Some(2), "expected exit 2 when no worktrees");

    let err = stderr(&out);
    assert!(err.contains("no worktrees"), "expected 'no worktrees' in:\n{err}");
}

#[test]
fn hook_failure_still_removes_but_exits_nonzero() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.add_hook("api", "post-remove", "echo 'cleanup failing'; exit 1");
    f.run_ok(&["new", "api", "feat/hookfail"]);

    let wt = f.path().join(".worktrees/feat-hookfail/api");
    let out = f.run(&["rm", "feat/hookfail"]);

    assert_eq!(out.status.code(), Some(1), "expected exit 1 when a hook fails");
    assert!(!wt.exists(), "worktree should be removed even when a hook fails");

    let err = stderr(&out);
    assert!(err.contains("hooks failed"), "expected failure summary in:\n{err}");
}

#[test]
fn down_and_post_remove_hooks_run_in_order() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.add_hook("api", "down", "echo 'DOWN-HOOK-RAN'");
    f.add_hook("api", "post-remove", "echo 'POST-REMOVE-RAN'");
    f.run_ok(&["new", "api", "feat/hooks"]);

    let out = f.run_ok(&["rm", "feat/hooks"]);
    let err = stderr(&out);

    let down_at = err.find("DOWN-HOOK-RAN").expect("down hook should run");
    let post_at = err.find("POST-REMOVE-RAN").expect("post-remove hook should run");
    assert!(down_at < post_at, "down should run before post-remove:\n{err}");
}

#[test]
fn unregistered_dir_in_task_dir_is_reported_not_torn_down() {
    // A directory under the task dir is not a worktree: it can be a hand-made
    // copy, or what an interrupted removal left behind (`drup clean` documents
    // both). Enumerating the task dir from the filesystem made `drup rm` treat one
    // as a repo — running teardown inside it and then failing `git worktree
    // remove` there, which aborted the whole op with some repos already gone and
    // left the branch permanently unremovable.
    let f = WorkspaceFixture::with_repos(&["api", "app"]);
    f.run_ok(&["new", "api,app", "feat/stray"]);

    let stray = f.path().join(".worktrees/feat-stray/api-backup");
    std::fs::create_dir_all(stray.join("src")).unwrap();

    let out = f.run(&["rm", "feat/stray"]);
    assert!(
        out.status.success(),
        "an unregistered dir must not fail the removal:\n{}",
        stderr(&out)
    );

    let err = stderr(&out);
    assert!(!f.path().join(".worktrees/feat-stray/api").exists(), "api not removed:\n{err}");
    assert!(!f.path().join(".worktrees/feat-stray/app").exists(), "app not removed:\n{err}");
    assert!(stray.is_dir(), "the unregistered dir must be left in place");
    assert!(
        err.contains("not registered with git"),
        "the leftover must be named, not silently ignored:\n{err}"
    );
}

#[test]
fn leftover_file_in_task_dir_does_not_fail() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/leftover"]);

    // A stray file in the task dir prevents rmdir — op should still succeed.
    std::fs::write(
        f.path().join(".worktrees/feat-leftover/.DS_Store"),
        "junk",
    )
    .unwrap();

    let out = f.run_ok(&["rm", "feat/leftover"]);
    let err = stderr(&out);
    assert!(err.contains("Removed worktrees"), "expected success summary in:\n{err}");
    assert!(!f.path().join(".worktrees/feat-leftover/api").exists());
}

// `drup list` enumerates from git, so `rm` must reach registrations whose directory is gone.
#[test]
fn rm_clears_a_registration_whose_directory_is_gone() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/stale"]);

    // Delete the working tree behind git's back, leaving the registration.
    std::fs::remove_dir_all(f.path().join(".worktrees/feat-stale/api")).unwrap();
    let registered = |f: &WorkspaceFixture| {
        String::from_utf8_lossy(&f.git_out("api", &["worktree", "list", "--porcelain"]).stdout)
            .contains("feat-stale")
    };
    assert!(registered(&f), "setup: git must still know the worktree");
    assert!(stderr(&f.run_ok(&["list"])).contains("feat-stale"), "setup: list must show it");

    let err = stderr(&f.run_ok(&["rm", "feat/stale"]));
    assert!(err.contains("Removed worktrees"), "expected success summary in:\n{err}");
    assert!(!registered(&f), "the stale registration must be pruned");
}

#[test]
fn keep_images_flag_toggles_ws_keep_images_on_post_remove() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.add_hook("api", "post-remove", "echo \"KEEP=${DRUP_KEEP_IMAGES:-unset}\"");

    f.run_ok(&["new", "api", "feat/prune-default"]);
    let err = stderr(&f.run_ok(&["rm", "feat/prune-default"]));
    assert!(err.contains("KEEP=unset"), "default rm must not set DRUP_KEEP_IMAGES:\n{err}");

    f.run_ok(&["new", "api", "feat/keep"]);
    let err = stderr(&f.run_ok(&["rm", "feat/keep", "--keep-images"]));
    assert!(err.contains("KEEP=1"), "--keep-images must set DRUP_KEEP_IMAGES=1:\n{err}");
}

#[test]
fn keep_volumes_flag_toggles_ws_keep_volumes_on_post_remove() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.add_hook("api", "post-remove", "echo \"KEEP=${DRUP_KEEP_VOLUMES:-unset}\"");

    f.run_ok(&["new", "api", "feat/vol-default"]);
    let err = stderr(&f.run_ok(&["rm", "feat/vol-default"]));
    assert!(err.contains("KEEP=unset"), "default rm must not set DRUP_KEEP_VOLUMES:\n{err}");

    f.run_ok(&["new", "api", "feat/vol-keep"]);
    let err = stderr(&f.run_ok(&["rm", "feat/vol-keep", "--keep-volumes"]));
    assert!(err.contains("KEEP=1"), "--keep-volumes must set DRUP_KEEP_VOLUMES=1:\n{err}");
}

#[test]
fn keep_flags_do_not_leak_into_down() {
    // `DRUP_KEEP_IMAGES` / `DRUP_KEEP_VOLUMES` are scoped to `post-remove` so a user's
    // declared `down` script can't accidentally read them as signals meant for the
    // later prune.
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.add_hook(
        "api",
        "down",
        "echo \"DOWN-KI=${DRUP_KEEP_IMAGES:-unset}\"; echo \"DOWN-KV=${DRUP_KEEP_VOLUMES:-unset}\"",
    );
    f.add_hook(
        "api",
        "post-remove",
        "echo \"POST-KI=${DRUP_KEEP_IMAGES:-unset}\"; echo \"POST-KV=${DRUP_KEEP_VOLUMES:-unset}\"",
    );
    f.run_ok(&["new", "api", "feat/scoped"]);
    let err = stderr(&f.run_ok(&["rm", "feat/scoped", "--keep-images", "--keep-volumes"]));
    assert!(err.contains("DOWN-KI=unset"), "DRUP_KEEP_IMAGES must not reach down:\n{err}");
    assert!(err.contains("DOWN-KV=unset"), "DRUP_KEEP_VOLUMES must not reach down:\n{err}");
    assert!(err.contains("POST-KI=1"), "DRUP_KEEP_IMAGES must reach post-remove:\n{err}");
    assert!(err.contains("POST-KV=1"), "DRUP_KEEP_VOLUMES must reach post-remove:\n{err}");
}
