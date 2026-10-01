mod common;

use common::{stderr, WorkspaceFixture};

/// Build a workspace with `api` carrying an `interactive: true` up hook that,
/// if it ever ran, would exit non-zero so we'd notice. The `ws new` runs with
/// `WS_NON_INTERACTIVE=1` so post-create (an "echo ready" we attach) doesn't
/// itself trip the refusal; `ws up` runs with the same env override so no-TTY
/// behavior is deterministic regardless of how `cargo test` was invoked.
fn fixture_with_interactive_up() -> WorkspaceFixture {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    std::fs::write(
        f.path().join(".ws.json"),
        r#"{
            "repos": [{
                "name": "api",
                "scripts": {
                    "up": {
                        "cmd": "echo SHOULD-NOT-RUN; exit 7",
                        "interactive": true
                    }
                }
            }]
        }"#,
    )
    .expect("write .ws.json");
    f
}

#[test]
fn ws_up_refuses_interactive_hook_without_a_tty() {
    let f = fixture_with_interactive_up();
    f.run_ok(&["new", "api", "feat/x"]);

    let out = f.run_with_env(&["up", "feat/x"], &[("WS_NON_INTERACTIVE", "1")]);
    let err = stderr(&out);

    assert_eq!(
        out.status.code(),
        Some(3),
        "interactive refused → exit 3, not generic hook failure:\nstderr:\n{err}"
    );
    assert!(
        err.contains("interactive hook refused") || err.contains("interactive terminal"),
        "refusal must name the condition in stderr:\n{err}"
    );
    assert!(
        err.contains("api"),
        "refusal must name the repo that needs the human:\n{err}"
    );
    assert!(
        !err.contains("SHOULD-NOT-RUN"),
        "refused hook must NOT have run its command:\n{err}"
    );
}

#[test]
fn ws_up_with_env_override_refuses_even_if_tty_present() {
    // `WS_NON_INTERACTIVE=1` is the forced-non-interactive escape hatch (CI,
    // scripted automation, debugging the refusal path locally). Verifies the
    // env check wins over any `/dev/tty` availability — ws must honor the
    // user's "I'm scripted, don't prompt" promise.
    let f = fixture_with_interactive_up();
    f.run_ok(&["new", "api", "feat/x"]);

    let out = f.run_with_env(&["up", "feat/x"], &[("WS_NON_INTERACTIVE", "1")]);
    assert_eq!(out.status.code(), Some(3));
}

#[test]
fn ws_up_mixes_failed_and_needs_human_as_a_generic_failure() {
    // One repo's hook refuses interactive, another's hook crashes outright.
    // The op must land as a generic failure (exit 1) rather than exit 3,
    // because exit 3 means "ONLY needs human" — a Claude wrapper watching
    // for 3-vs-1 would otherwise retry assuming nothing else is broken.
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.add_repo("app");
    std::fs::write(
        f.path().join(".ws.json"),
        r#"{
            "repos": [
                {
                    "name": "api",
                    "scripts": { "up": {"cmd": "echo nope; exit 7", "interactive": true} }
                },
                {
                    "name": "app",
                    "scripts": { "up": "echo CRASHING; exit 1" }
                }
            ]
        }"#,
    )
    .expect("write .ws.json");
    f.run_ok(&["new", "api,app", "feat/x"]);

    let out = f.run_with_env(&["up", "feat/x"], &[("WS_NON_INTERACTIVE", "1")]);
    let err = stderr(&out);

    assert_eq!(
        out.status.code(),
        Some(1),
        "mixed failures → generic exit 1, not exit 3:\nstderr:\n{err}"
    );
    assert!(err.contains("app"), "generic-failure message must name `app`:\n{err}");
    assert!(err.contains("api"), "needs-human group must still name `api`:\n{err}");
}

#[test]
fn non_interactive_hook_is_unaffected_by_the_env_override() {
    // Regression: `WS_NON_INTERACTIVE` only gates the interactive-hook path.
    // A plain non-interactive hook under the same env must still run — if we
    // ever broaden the check and start refusing non-interactive hooks too,
    // we'd break every CI/Claude invocation of `ws up`.
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.add_hook("api", "up", "echo ran-non-interactive");
    f.run_ok(&["new", "api", "feat/x"]);

    let out = f.run_with_env(&["up", "feat/x"], &[("WS_NON_INTERACTIVE", "1")]);
    assert!(out.status.success(), "non-interactive hook must still run under WS_NON_INTERACTIVE");
    assert!(stderr(&out).contains("ran-non-interactive"));
}
