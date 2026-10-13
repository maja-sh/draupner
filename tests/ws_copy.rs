//! `copy`: untracked dev files (certs/keys/env) named in `.ws.json` are seeded
//! from the main checkout into each new worktree at `wsx new`.

mod common;

use common::{stderr, WorkspaceFixture};
use std::fs;

#[test]
fn ws_new_seeds_copy_entries_into_the_worktree() {
    let f = WorkspaceFixture::empty();
    let repo = f.add_repo("api");

    fs::write(repo.join(".env"), "SECRET=1").unwrap();
    fs::create_dir_all(repo.join("build-dev/cert")).unwrap();
    fs::write(repo.join("build-dev/cert/dev.pem"), "cert").unwrap();
    fs::create_dir_all(repo.join("storage")).unwrap();
    fs::write(repo.join("storage/oauth-private.key"), "key").unwrap();
    fs::write(repo.join("storage/ignore.txt"), "no").unwrap();

    f.write_ws_json(
        r#"{"repos":[{"name":"api","copy":["build-dev/cert",".env","storage/oauth-*.key"]}]}"#,
    );

    let out = f.run_ok(&["new", "api", "feat/foo"]);
    assert!(out.status.success(), "wsx new failed:\n{}", stderr(&out));

    let wt = f.path().join(".worktrees/feat-foo/api");
    assert_eq!(fs::read_to_string(wt.join(".env")).unwrap(), "SECRET=1");
    assert!(wt.join("build-dev/cert/dev.pem").is_file(), "directory copied recursively");
    assert_eq!(fs::read_to_string(wt.join("storage/oauth-private.key")).unwrap(), "key");
    // The glob must not pull siblings that don't match.
    assert!(!wt.join("storage/ignore.txt").exists());
}

#[test]
fn copied_files_do_not_leave_the_worktree_dirty() {
    let f = WorkspaceFixture::empty();
    let repo = f.add_repo("api");

    // A seeded file the repo does NOT gitignore — without an exclude entry this
    // shows up as untracked (`??`) and every `wsx list` reads the worktree dirty.
    fs::write(repo.join("dev-secrets.txt"), "s").unwrap();
    fs::create_dir_all(repo.join("build-dev/cert")).unwrap();
    fs::write(repo.join("build-dev/cert/dev.pem"), "cert").unwrap();

    f.write_ws_json(
        r#"{"repos":[{"name":"api","copy":["dev-secrets.txt","build-dev/cert"]}]}"#,
    );

    let out = f.run_ok(&["new", "api", "feat/foo"]);
    assert!(out.status.success(), "wsx new failed:\n{}", stderr(&out));

    let wt = f.path().join(".worktrees/feat-foo/api");
    assert!(wt.join("dev-secrets.txt").is_file(), "file was seeded");

    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&wt)
        .args(["status", "--porcelain"])
        .output()
        .expect("git status");
    assert_eq!(
        String::from_utf8_lossy(&status.stdout).trim(),
        "",
        "worktree must read clean after copy"
    );
}
