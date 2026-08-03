mod common;

use common::{stderr, WorkspaceFixture};

#[test]
fn init_local_writes_config_and_hook_templates() {
    let ws = WorkspaceFixture::empty();
    ws.add_repo("api");
    ws.add_repo("app");

    let out = ws.run_ok(&["init"]);
    let err = stderr(&out);
    assert!(err.contains("ws-init"));
    assert!(err.contains(".ws.json"));
    assert!(err.contains(".ws.schema.json"));
    assert!(err.contains("ws claude"), "success nudge should mention `ws claude`, got: {err}");
    assert!(!err.contains("ws register"), "success nudge must not mention the old `ws register` name");

    assert!(ws.path().join(".ws.json").is_file());
    assert!(ws.path().join(".ws.schema.json").is_file());
    let cfg = std::fs::read_to_string(ws.path().join(".ws.json")).unwrap();
    assert!(cfg.contains("\"api\""));
    assert!(cfg.contains("\"app\""));
    assert!(cfg.contains("$schema"));

    // A scaffolded template is inert until `.ws.json` names it, so each body has
    // to carry the `scripts` entry that turns it on.
    for repo in ["api", "app"] {
        for event in ["prepare", "post-create", "up", "down", "post-remove", "status"] {
            let p = ws.path().join(".ws-hooks").join(format!("{repo}-{event}.sh"));
            assert!(p.is_file(), "expected hook template {p:?}");
            let body = std::fs::read_to_string(&p).unwrap();
            assert!(
                body.contains(&format!("\"{event}\": \".ws-hooks/{repo}-{event}.sh\"")),
                "template should name the `scripts` entry that enables it: {body}"
            );
            assert!(!body.contains("$1"), "the removed $1 contract must not be taught: {body}");
        }
    }
}

#[test]
fn init_remote_writes_the_remote_block() {
    let ws = WorkspaceFixture::empty();
    ws.add_repo("api");
    // Passes the client-readiness check: mutagen "installed", host reachable.
    let ok = ws.stub("ok", "exit 0");

    ws.run_env_ok(
        &["init", "--remote"],
        &[
            ("WS_REMOTE_HOST", "srv"),
            ("WS_SSH", ok.to_str().unwrap()),
            ("WS_MUTAGEN", ok.to_str().unwrap()),
        ],
    );

    let cfg = std::fs::read_to_string(ws.path().join(".ws.json")).unwrap();
    assert!(cfg.contains("\"remote\""), "expected a remote block:\n{cfg}");
    assert!(cfg.contains("srv"), "expected the connection:\n{cfg}");
    assert!(cfg.contains("\"connection\""));
    // workspace_path is written explicitly, defaulting to the workspace dir name.
    let base = ws.path().file_name().unwrap().to_str().unwrap();
    assert!(cfg.contains(base), "expected workspace_path default of `{base}`:\n{cfg}");
}

#[test]
fn init_remote_refuses_an_unreachable_host_by_default() {
    let ws = WorkspaceFixture::empty();
    ws.add_repo("api");
    let ok = ws.stub("ok", "exit 0");
    let bad = ws.stub("bad-ssh", "exit 1");

    let out = ws.run_with_env(
        &["init", "--remote", "srv"],
        &[("WS_MUTAGEN", ok.to_str().unwrap()), ("WS_SSH", bad.to_str().unwrap())],
    );
    assert_eq!(out.status.code(), Some(2), "should refuse:\n{}", stderr(&out));
    assert!(stderr(&out).contains("reach"), "clear message:\n{}", stderr(&out));
    assert!(!ws.path().join(".ws.json").exists(), "no config written on refusal");
}

#[test]
fn init_remote_refuses_when_mutagen_missing() {
    let ws = WorkspaceFixture::empty();
    ws.add_repo("api");
    let ok = ws.stub("ok", "exit 0");
    let bad = ws.stub("no-mutagen", "exit 1");

    let out = ws.run_with_env(
        &["init", "--remote", "srv"],
        &[("WS_SSH", ok.to_str().unwrap()), ("WS_MUTAGEN", bad.to_str().unwrap())],
    );
    assert_eq!(out.status.code(), Some(2), "should refuse:\n{}", stderr(&out));
    assert!(stderr(&out).contains("mutagen"), "clear message:\n{}", stderr(&out));
    assert!(!ws.path().join(".ws.json").exists());
}

#[test]
fn init_remote_no_verify_skips_the_checks() {
    let ws = WorkspaceFixture::empty();
    ws.add_repo("api");
    let bad = ws.stub("bad", "exit 1");

    ws.run_env_ok(
        &["init", "--remote", "srv", "--no-verify"],
        &[("WS_SSH", bad.to_str().unwrap()), ("WS_MUTAGEN", bad.to_str().unwrap())],
    );
    assert!(ws.path().join(".ws.json").exists(), "config written with --no-verify");
}

#[test]
fn init_without_remote_writes_no_remote_block() {
    let ws = WorkspaceFixture::empty();
    ws.add_repo("api");
    ws.run_ok(&["init"]);
    let cfg = std::fs::read_to_string(ws.path().join(".ws.json")).unwrap();
    assert!(!cfg.contains("\"remote\""), "no remote block expected without --remote:\n{cfg}");
}

#[test]
fn init_refuses_when_ws_json_exists() {
    let ws = WorkspaceFixture::empty();
    ws.add_repo("api");
    std::fs::write(ws.path().join(".ws.json"), "{}").unwrap();

    let out = ws.run(&["init"]);
    assert!(!out.status.success(), "expected failure when .ws.json exists");
    assert!(stderr(&out).contains(".ws.json"));
}

#[test]
fn init_skips_dotfile_directories() {
    let ws = WorkspaceFixture::empty();
    ws.add_repo("keeper");
    let hidden = ws.path().join(".hidden");
    std::fs::create_dir_all(&hidden).unwrap();
    std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&hidden)
        .output()
        .unwrap();

    ws.run_ok(&["init"]);
    let cfg = std::fs::read_to_string(ws.path().join(".ws.json")).unwrap();
    assert!(cfg.contains("keeper"));
    assert!(!cfg.contains(".hidden"));
}

#[test]
fn refresh_schema_only_rewrites_schema_file() {
    let ws = WorkspaceFixture::empty();
    ws.add_repo("api");
    ws.run_ok(&["init"]);

    let cfg_before = std::fs::read_to_string(ws.path().join(".ws.json")).unwrap();
    std::fs::write(ws.path().join(".ws.schema.json"), "STALE").unwrap();

    ws.run_ok(&["init", "--refresh-schema"]);

    let schema = std::fs::read_to_string(ws.path().join(".ws.schema.json")).unwrap();
    assert_ne!(schema, "STALE");
    assert!(schema.contains("$schema"));
    let cfg_after = std::fs::read_to_string(ws.path().join(".ws.json")).unwrap();
    assert_eq!(cfg_before, cfg_after, ".ws.json should be untouched by --refresh-schema");
}

#[test]
fn refresh_schema_refuses_outside_workspace() {
    let ws = WorkspaceFixture::empty();
    let out = ws.run(&["init", "--refresh-schema"]);
    assert!(!out.status.success(), "expected failure outside a workspace");
    assert!(stderr(&out).contains(".ws.json"));
    assert!(!ws.path().join(".ws.schema.json").exists(), "should not create orphan schema file");
}

#[test]
fn refresh_schema_works_from_a_subdirectory() {
    // Every other op locates the workspace by walking up, so this one must too.
    let ws = WorkspaceFixture::empty();
    ws.add_repo("api");
    ws.run_ok(&["init"]);
    std::fs::write(ws.path().join(".ws.schema.json"), "STALE").unwrap();

    let out = ws.run_in("api", &["init", "--refresh-schema"]);
    assert!(out.status.success(), "should locate the workspace from a subdir:\n{}", stderr(&out));

    let schema = std::fs::read_to_string(ws.path().join(".ws.schema.json")).unwrap();
    assert_ne!(schema, "STALE", "the schema at the workspace root should be refreshed");
    assert!(
        !ws.path().join("api/.ws.schema.json").exists(),
        "the schema belongs at the workspace root, not the cwd"
    );
}

#[test]
fn init_refuses_inside_an_enclosing_workspace() {
    let ws = WorkspaceFixture::with_repos(&["api"]);
    let sub = ws.path().join("nested");
    std::fs::create_dir_all(&sub).unwrap();

    let out = ws.run_in("nested", &["init"]);
    assert_eq!(out.status.code(), Some(2), "expected preflight exit 2");
    let err = stderr(&out);
    assert!(err.contains("already inside the workspace"), "expected nesting refusal in:\n{err}");
    assert!(!sub.join(".ws.json").exists(), "must not write a nested .ws.json");
}

#[test]
fn init_refuses_inside_worktree_tree_with_sharper_message() {
    let ws = WorkspaceFixture::with_repos(&["api"]);
    ws.run_ok(&["new", "api", "feat/x"]);

    let out = ws.run_in(".worktrees/feat-x/api", &["init"]);
    assert_eq!(out.status.code(), Some(2), "expected preflight exit 2");
    assert!(
        stderr(&out).contains(".worktrees/"),
        "expected the sharper worktree-tree message: {}",
        stderr(&out)
    );
}

#[test]
fn init_skips_symlinked_directories() {
    let ws = WorkspaceFixture::empty();
    let real = ws.add_repo("real");

    // Skipping the symlink is what stops the same repo being registered twice.
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real, ws.path().join("linked")).unwrap();

    ws.run_ok(&["init"]);
    let cfg = std::fs::read_to_string(ws.path().join(".ws.json")).unwrap();
    assert!(cfg.contains("\"real\""));
    assert!(!cfg.contains("\"linked\""), "symlinked entries must be skipped");
}
