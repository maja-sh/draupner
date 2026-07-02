mod common;

use common::{stderr, WorkspaceFixture};

#[test]
fn claude_writes_all_skills() {
    let ws = WorkspaceFixture::empty();
    let skills_dir = ws.path().join("skills-out");

    let bin = env!("CARGO_BIN_EXE_ws");
    let out = std::process::Command::new(bin)
        .args(["claude"])
        .env("CLAUDE_SKILLS_DIR", &skills_dir)
        .env_remove("RUST_LOG")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));

    for name in [
        "ws-new", "ws-up", "ws-down", "ws-rm", "ws-list", "ws-here",
        "ws-use", "ws-clean", "ws-init",
    ] {
        let p = skills_dir.join(name).join("SKILL.md");
        assert!(p.is_file(), "missing {p:?}");
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(body.starts_with("---\n"), "{name}: no frontmatter");
        assert!(body.contains(&format!("name: {name}")));
        assert!(body.contains("Invoke via bash"));
    }
}

#[test]
fn claude_is_idempotent() {
    let ws = WorkspaceFixture::empty();
    let skills_dir = ws.path().join("skills-out");
    let bin = env!("CARGO_BIN_EXE_ws");

    for _ in 0..2 {
        let out = std::process::Command::new(bin)
            .args(["claude"])
            .env("CLAUDE_SKILLS_DIR", &skills_dir)
            .env_remove("RUST_LOG")
            .output()
            .unwrap();
        assert!(out.status.success());
    }
    assert!(skills_dir.join("ws-new").join("SKILL.md").is_file());
}
