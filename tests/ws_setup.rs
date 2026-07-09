mod common;

use common::{stderr, WorkspaceFixture};

// On a Linux host, `ws setup` reports the privileged gaps and points at the
// versioned setup script rather than driving `sudo` itself. The fixture runs
// with an externally-managed Caddy (`WS_CADDY_ADMIN`), so the rootless step is a
// no-op and no real infra is touched; DNS/CA still probe as missing.
#[test]
fn setup_on_linux_reports_gaps_and_points_at_the_script() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(r#"{"domain":"myapp.test","repos":["api"]}"#);

    let out = f.run_ok(&["setup"]);
    let err = stderr(&out);

    // Rootless Caddy step ran (externally managed → ok).
    assert!(err.contains("rootless network + Caddy"), "expected the rootless step:\n{err}");
    // Privileged gaps are reported…
    assert!(err.contains("wildcard DNS"), "expected the DNS status line:\n{err}");
    assert!(err.contains("CA trust"), "expected the CA-trust status line:\n{err}");
    // …and the user is pointed at the transparent setup script (not sudo'd).
    assert!(
        err.contains("server-setup/debian.sh"),
        "Linux setup must point at the distro script:\n{err}"
    );
}

// With no `domain`, routing isn't configured, so there are no privileged bits —
// `ws setup` says so and exits cleanly.
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

// `ws setup --port 443` persists the opt-in port; the probe then flags the
// privileged-port gap (the test box's `ip_unprivileged_port_start` is > 443).
#[test]
fn setup_port_443_persists_and_flags_the_sysctl_gap() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(r#"{"domain":"myapp.test","repos":["api"]}"#);

    let out = f.run_ok(&["setup", "--port", "443"]);
    let err = stderr(&out);

    assert!(err.contains("HTTPS port set to 443"), "port opt-in should be persisted:\n{err}");
    assert!(err.contains("privileged port"), "expected the privileged-port status line:\n{err}");
    // The persisted port file lands under the (fixture) HOME config dir.
    assert!(
        f.path().join(".config/ws/https_port").exists(),
        "https_port must be persisted under ~/.config/ws"
    );
}
