//! `copy`: untracked dev files (certs/keys/env) named in `.ws.json` are seeded
//! from the main checkout into each new worktree at `ws new`.

mod common;

use common::{stderr, WorkspaceFixture};
use std::fs;

#[test]
fn ws_new_seeds_copy_entries_into_the_worktree() {
    let f = WorkspaceFixture::empty();
    let repo = f.add_repo("api");

    // Untracked dev files that live only in the main checkout.
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
    assert!(out.status.success(), "ws new failed:\n{}", stderr(&out));

    let wt = f.path().join(".worktrees/feat-foo/api");
    // File, recursive directory, and glob all land at the same relative path.
    assert_eq!(fs::read_to_string(wt.join(".env")).unwrap(), "SECRET=1");
    assert!(wt.join("build-dev/cert/dev.pem").is_file(), "directory copied recursively");
    assert_eq!(fs::read_to_string(wt.join("storage/oauth-private.key")).unwrap(), "key");
    // The glob must not pull siblings that don't match.
    assert!(!wt.join("storage/ignore.txt").exists());
}
