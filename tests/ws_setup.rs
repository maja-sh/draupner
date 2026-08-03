mod common;

use common::{stderr, WorkspaceFixture};

// Linux-only: on macOS the same command walks the interactive `[R]un/[S]kip`
// prompt and never prints the script path, so these assertions don't apply.
#[cfg(target_os = "linux")]
#[test]
fn setup_on_linux_reports_gaps_and_points_at_the_script() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(r#"{"domain":"myapp.test","repos":["api"]}"#);

    let out = f.run_ok(&["setup"]);
    let err = stderr(&out);

    assert!(err.contains("rootless network + Caddy"), "expected the rootless step:\n{err}");
    assert!(err.contains("wildcard DNS"), "expected the DNS status line:\n{err}");
    assert!(err.contains("CA trust"), "expected the CA-trust status line:\n{err}");
    // Privileged work is pointed at, never driven under sudo by `ws` itself.
    assert!(
        err.contains("server-setup/debian.sh"),
        "Linux setup must point at the distro script:\n{err}"
    );
}

#[test]
fn setup_without_a_domain_is_a_clean_noop() {
    let f = WorkspaceFixture::with_repos(&["api"]);

    let out = f.run_ok(&["setup"]);
    assert!(
        stderr(&out).contains("No `domain` set"),
        "expected the no-domain note:\n{}",
        stderr(&out)
    );
}

// Assumes the test box's `ip_unprivileged_port_start` is above 443, which is
// what makes the gap show up at all.
#[test]
fn setup_port_443_persists_and_flags_the_sysctl_gap() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(r#"{"domain":"myapp.test","repos":["api"]}"#);

    let out = f.run_ok(&["setup", "--port", "443"]);
    let err = stderr(&out);

    assert!(err.contains("HTTPS port set to 443"), "port opt-in should be persisted:\n{err}");
    assert!(err.contains("privileged port"), "expected the privileged-port status line:\n{err}");
    assert!(
        f.path().join(".config/ws/https_port").exists(),
        "https_port must be persisted under ~/.config/ws"
    );
}
