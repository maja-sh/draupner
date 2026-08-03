mod common;

use common::{stderr, WorkspaceFixture};

/// `ws remote check` makes two ssh calls: a reachability probe whose last arg is
/// `true`, then the probe script — hence the branch. `results` stands in for what
/// the server reports, as `<key>:<ok|fail>` lines.
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
    let ssh = ssh_stub(&f, "mutagen:ok\\npodman:ok\\ncaddy:ok\\nworkspace:ok\\ndns:ok\\n");
    let mutagen = f.stub("mutagen-fake", "exit 0");

    let out = f.run_env_ok(
        &["remote", "check"],
        &[("WS_SSH", ssh.to_str().unwrap()), ("WS_MUTAGEN", mutagen.to_str().unwrap())],
    );
    let err = stderr(&out);
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
    let ssh = ssh_stub(&f, "mutagen:ok\\npodman:fail\\ncaddy:ok\\nworkspace:ok\\ndns:ok\\n");
    let mutagen = f.stub("mutagen-fake", "exit 0");

    let out = f.run_with_env(
        &["remote", "check"],
        &[("WS_SSH", ssh.to_str().unwrap()), ("WS_MUTAGEN", mutagen.to_str().unwrap())],
    );
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "a failing probe must exit 1:\n{err}");
    assert!(err.contains("rootless podman is up"), "expected the podman probe line:\n{err}");
    assert!(err.contains("MISSING") || err.contains("✗"), "podman should read as failed:\n{err}");
}

#[test]
fn remote_check_flags_a_server_that_cannot_resolve_its_own_domain() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(remote_ws_json());
    let ssh = ssh_stub(&f, "mutagen:ok\\npodman:ok\\ncaddy:ok\\nworkspace:ok\\ndns:fail\\n");
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

#[test]
fn remote_check_without_a_domain_omits_the_dns_probe() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(
        r#"{"runtime":"podman","remote":{"connection":"srv","workspace_path":"myapp"},"repos":["api"]}"#,
    );
    // Only the unconditional keys: a missing `dns` must not fail a workspace that
    // never asked for routing.
    let ssh = ssh_stub(&f, "mutagen:ok\\npodman:ok\\ncaddy:ok\\nworkspace:ok\\n");
    let mutagen = f.stub("mutagen-fake", "exit 0");

    let out = f.run_env_ok(
        &["remote", "check"],
        &[("WS_SSH", ssh.to_str().unwrap()), ("WS_MUTAGEN", mutagen.to_str().unwrap())],
    );
    let err = stderr(&out);
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
    // No remote block, so the workspace dir name becomes the worktree base.
    f.write_ws_json(r#"{"domain":"myapp.test","repos":["api"]}"#);
    let ssh = ssh_stub(&f, "mutagen:ok\\npodman:ok\\ncaddy:ok\\nworkspace:ok\\ndns:ok\\n");
    let mutagen = f.stub("mutagen-fake", "exit 0");

    let out = f.run_env_ok(
        &["remote", "check", "--host", "otherbox"],
        &[("WS_SSH", ssh.to_str().unwrap()), ("WS_MUTAGEN", mutagen.to_str().unwrap())],
    );
    let err = stderr(&out);
    assert!(err.contains("otherbox:"), "expected the --host target in the path echo:\n{err}");
}

#[test]
fn remote_check_local_mode_reports_gaps() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(r#"{"domain":"myapp.test","repos":["api"]}"#);

    // A dead admin endpoint is what makes the CA probe fail cleanly.
    let out = f.run_with_env(&["remote", "check"], &[("WS_CADDY_ADMIN", "http://127.0.0.1:1")]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "local gaps must exit 1:\n{err}");
    assert!(err.contains("wildcard DNS"), "expected the local DNS status line:\n{err}");
    assert!(err.contains("ws setup"), "expected the `ws setup` nudge:\n{err}");
}

// The argument-injection guard: a host ssh would parse as an option must be
// refused before any ssh runs.
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
