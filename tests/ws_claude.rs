mod common;

use common::{stderr, WorkspaceFixture};

#[test]
fn claude_writes_all_skills() {
    let ws = WorkspaceFixture::empty();
    let out = ws.run_ok(&["claude"]);
    assert!(stderr(&out).contains("ws-claude"));

    let skills_dir = ws.path().join("claude-skills");
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
    for _ in 0..2 {
        ws.run_ok(&["claude"]);
    }
    assert!(ws.path().join("claude-skills").join("ws-new").join("SKILL.md").is_file());
}

#[test]
fn claude_fails_helpfully_when_home_missing() {
    let ws = WorkspaceFixture::empty();
    // Force both HOME and CLAUDE_SKILLS_DIR to be missing. Since the fixture
    // sets both by default, we override with empty strings via a different
    // path: spawn manually so we can `.env_remove`.
    let bin = env!("CARGO_BIN_EXE_ws");
    let out = std::process::Command::new(bin)
        .args(["claude"])
        .current_dir(ws.path())
        .env_clear()
        // clap needs a minimum env to render usage/help; PATH avoids missing exec look-ups.
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .output()
        .expect("spawn");
    assert!(!out.status.success(), "expected failure without HOME");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("HOME is not set"),
        "expected 'HOME is not set' in stderr, got: {err}"
    );
}
