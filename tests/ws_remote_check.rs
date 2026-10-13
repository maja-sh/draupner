mod common;

use common::{stderr, WorkspaceFixture};

/// Fake `ssh` answering both `wsx remote check` invocations: the reachability probe (last arg `true` exits 0) and the server-probe script printing `<key>:<ok|fail>` lines.
fn ssh_stub(f: &WorkspaceFixture, results: &str) -> std::path::PathBuf {
    f.stub(
        "ssh-fake",
        &format!(
            r#"last="${{@: -1}}"
if [ "$last" = "true" ]; then exit 0; fi
printf '{results}'"#
        ),
    )
}

fn remote_ws_json() -> &'static str {
    r#"{"domain":"myapp.test","runtime":"podman",
        "remote":{"connection":"srv","workspace_path":"myapp"},
        "repos":["api"]}"#
}

#[test]
fn remote_check_all_green_reports_ready() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(remote_ws_json());
    let ssh = ssh_stub(&f, "mutagen:ok\\nruntime:ok\\ncaddy:ok\\nworkspace:ok\\ndns:ok\\n");
    let mutagen = f.stub("mutagen-fake", "exit 0");

    let out = f.run_with_env(
        &["remote", "check"],
        &[("WS_SSH", ssh.to_str().unwrap()), ("WS_MUTAGEN", mutagen.to_str().unwrap())],
    );
    let err = stderr(&out);
    assert!(out.status.success(), "healthy remote must exit 0:\n{err}");
    assert!(err.contains("remote looks ready"), "expected ready summary:\n{err}");
    assert!(
        err.contains("srv:myapp/<task>/<repo>"),
        "expected the worktree path echo:\n{err}"
    );
}

#[test]
fn remote_check_flags_a_failing_server_probe() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(remote_ws_json());
    let ssh = ssh_stub(&f, "mutagen:ok\\nruntime:fail\\ncaddy:ok\\nworkspace:ok\\ndns:ok\\n");
    let mutagen = f.stub("mutagen-fake", "exit 0");

    let out = f.run_with_env(
        &["remote", "check"],
        &[("WS_SSH", ssh.to_str().unwrap()), ("WS_MUTAGEN", mutagen.to_str().unwrap())],
    );
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "a failing probe must exit 1:\n{err}");
    // The line names the runtime the workspace configured, not a fixed `podman`.
    assert!(err.contains("podman is up on the server"), "expected the runtime probe line:\n{err}");
    assert!(err.contains("MISSING") || err.contains("✗"), "the runtime should read as failed:\n{err}");
}

#[test]
fn remote_check_flags_a_server_that_cannot_resolve_its_own_domain() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(remote_ws_json());
    let ssh = ssh_stub(&f, "mutagen:ok\\nruntime:ok\\ncaddy:ok\\nworkspace:ok\\ndns:fail\\n");
    let mutagen = f.stub("mutagen-fake", "exit 0");

    let out = f.run_with_env(
        &["remote", "check"],
        &[("WS_SSH", ssh.to_str().unwrap()), ("WS_MUTAGEN", mutagen.to_str().unwrap())],
    );
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "unresolvable domain must exit 1:\n{err}");
    assert!(err.contains("ws-probe.myapp.test"), "expected the name that failed:\n{err}");
    assert!(err.contains("server-setup/debian.sh"), "expected the pointer at the fix:\n{err}");
}

// No `domain` — the DNS check must be absent rather than present-and-passing.
#[test]
fn remote_check_without_a_domain_omits_the_dns_probe() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(
        r#"{"runtime":"podman","remote":{"connection":"srv","workspace_path":"myapp"},"repos":["api"]}"#,
    );
    // A missing `dns` key must not fail a workspace that never asked for routing.
    let ssh = ssh_stub(&f, "mutagen:ok\\nruntime:ok\\ncaddy:ok\\nworkspace:ok\\n");
    let mutagen = f.stub("mutagen-fake", "exit 0");

    let out = f.run_with_env(
        &["remote", "check"],
        &[("WS_SSH", ssh.to_str().unwrap()), ("WS_MUTAGEN", mutagen.to_str().unwrap())],
    );
    let err = stderr(&out);
    assert!(out.status.success(), "a domainless remote must still pass:\n{err}");
    assert!(!err.contains("dns"), "the DNS probe must not appear at all:\n{err}");
}

#[test]
fn remote_check_unreachable_ssh_skips_server_probes() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(remote_ws_json());
    let ssh = f.stub("ssh-fake", "exit 1");
    let mutagen = f.stub("mutagen-fake", "exit 0");

    let out = f.run_with_env(
        &["remote", "check"],
        &[("WS_SSH", ssh.to_str().unwrap()), ("WS_MUTAGEN", mutagen.to_str().unwrap())],
    );
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "unreachable remote must exit 1:\n{err}");
    assert!(err.contains("SSH isn't reachable"), "expected the skip note:\n{err}");
}

#[test]
fn remote_check_host_flag_overrides_and_defaults_the_path() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    // No remote block; the workspace dir name becomes the default worktree base.
    f.write_ws_json(r#"{"domain":"myapp.test","repos":["api"]}"#);
    let ssh = ssh_stub(&f, "mutagen:ok\\nruntime:ok\\ncaddy:ok\\nworkspace:ok\\ndns:ok\\n");
    let mutagen = f.stub("mutagen-fake", "exit 0");

    let out = f.run_with_env(
        &["remote", "check", "--host", "otherbox"],
        &[("WS_SSH", ssh.to_str().unwrap()), ("WS_MUTAGEN", mutagen.to_str().unwrap())],
    );
    let err = stderr(&out);
    assert!(out.status.success(), "--host healthy check must pass:\n{err}");
    assert!(err.contains("otherbox:"), "expected the --host target in the path echo:\n{err}");
}

// Local mode: the fixture's dead Caddy admin makes DNS + CA read as gaps, so exit is 1.
#[test]
fn remote_check_local_mode_reports_gaps() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(r#"{"domain":"myapp.test","repos":["api"]}"#);

    // Point the Caddy admin at a dead endpoint so the CA probe fails cleanly.
    let out = f.run_with_env(&["remote", "check"], &[("WS_CADDY_ADMIN", "http://127.0.0.1:1")]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "local gaps must exit 1:\n{err}");
    assert!(err.contains("wildcard DNS"), "expected the local DNS status line:\n{err}");
    assert!(err.contains("wsx setup"), "expected the `wsx setup` nudge:\n{err}");
}

// Argument-injection guard: an option-shaped `--host` (leading dash) is refused before any ssh runs.
#[test]
fn remote_check_rejects_an_option_shaped_host() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    let out = f.run(&["remote", "check", "--host=-oProxyCommand=touch /tmp/pwned"]);
    assert_eq!(out.status.code(), Some(2), "unsafe --host must be a preflight error:\n{}", stderr(&out));
    assert!(stderr(&out).contains("unsafe SSH host"), "expected the guard message:\n{}", stderr(&out));
}

#[test]
fn remote_check_local_without_a_domain_is_a_clean_noop() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    let out = f.run_ok(&["remote", "check"]);
    assert!(
        stderr(&out).contains("no `domain` set"),
        "expected the no-domain note:\n{}",
        stderr(&out)
    );
}
