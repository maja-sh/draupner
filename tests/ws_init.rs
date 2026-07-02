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
    // The success nudge must point at the current op name, not the renamed one.
    assert!(err.contains("ws claude"), "success nudge should mention `ws claude`, got: {err}");
    assert!(!err.contains("ws register"), "success nudge must not mention the old `ws register` name");

    assert!(ws.path().join(".ws.json").is_file());
    assert!(ws.path().join(".ws.schema.json").is_file());
    let cfg = std::fs::read_to_string(ws.path().join(".ws.json")).unwrap();
    assert!(cfg.contains("\"api\""));
    assert!(cfg.contains("\"app\""));
    assert!(cfg.contains("$schema"));

    for repo in ["api", "app"] {
        for event in ["post-create", "up", "down", "post-remove", "status", "test"] {
            let p = ws.path().join(".ws-hooks").join(repo).join(event);
            assert!(p.is_file(), "expected hook template {p:?}");
            let body = std::fs::read_to_string(&p).unwrap();
            assert!(body.contains(&format!("{repo}/{event}")), "template body mismatch: {body}");
        }
    }
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
    // A `.hidden` git repo should not be picked up as a workspace repo.
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
    // No `.ws.json` present — refresh-schema shouldn't scatter an orphan schema.
    let out = ws.run(&["init", "--refresh-schema"]);
    assert!(!out.status.success(), "expected failure outside a workspace");
    assert!(stderr(&out).contains(".ws.json"));
    assert!(!ws.path().join(".ws.schema.json").exists(), "should not create orphan schema file");
}

#[test]
fn init_skips_symlinked_directories() {
    let ws = WorkspaceFixture::empty();
    let real = ws.add_repo("real");

    // Symlink `linked` -> `real`. The symlink should be skipped so the same
    // repo isn't registered under two names.
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real, ws.path().join("linked")).unwrap();

    ws.run_ok(&["init"]);
    let cfg = std::fs::read_to_string(ws.path().join(".ws.json")).unwrap();
    assert!(cfg.contains("\"real\""));
    assert!(!cfg.contains("\"linked\""), "symlinked entries must be skipped");
}
