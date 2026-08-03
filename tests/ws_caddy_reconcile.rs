mod common;

use common::{stderr, WorkspaceFixture};
use std::io::{Read, Write};
use std::net::TcpListener;

// These tests drive the reconciliation block in `caddy::ensure_running` — the
// code that may remove a *running* shared Caddy. `WS_CADDY_MANAGE=managed` keeps
// the container lifecycle live while the admin API is a local stub, and the
// container runtime is a fake that records every argv, so the assertions are
// about what `ws` would have destroyed.

/// A free port for this run's HTTPS side, so nothing depends on the machine's
/// real :8443 being free (this suite runs on boxes that serve real stacks).
/// Bound-then-dropped, so a parallel test could steal it back — accepted, the
/// window is milliseconds and `preflight_ports` failing would name it clearly.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").expect("probe a free port").local_addr().unwrap().port()
}

/// A path-aware admin stub: `listen` and `routes` answer with real JSON so the
/// reconciliation match takes the address arm rather than the wedged-admin arm.
/// Leaked threads are fine — the process ends with the test binary.
fn admin_stub(https_port: u16, routes_json: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind admin stub");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut buf = [0u8; 2048];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req.split_whitespace().nth(1).unwrap_or("/");
            let listen = format!(r#"[":{https_port}"]"#);
            let body = if path.ends_with("/listen") {
                &listen
            } else if path.ends_with("/routes") {
                routes_json
            } else {
                "{}"
            };
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// A workspace whose runtime is a fake that reports a running Caddy published on
/// `published`, and appends every argv line to `RT_LOG`.
fn fixture_with_fake_runtime(published: &str) -> (WorkspaceFixture, std::path::PathBuf) {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    let rt = f.stub(
        "fake-runtime",
        r#"printf '%s\n' "$*" >> "$RT_LOG"
case "$1" in
  container) echo true ;;
  port) echo "$RT_PUBLISHED" ;;
esac
exit 0"#,
    );
    f.write_ws_json(&format!(
        r#"{{"domain":"myapp.test","runtime":"{}","repos":["api"]}}"#,
        rt.display()
    ));
    f.run_ok(&["new", "api", "feat/x"]);
    let log = f.path().join("rt.log");
    // The published address rides an env var so one script serves every test.
    std::fs::write(&log, "").unwrap();
    let _ = published; // recorded per-run via RT_PUBLISHED below
    (f, log)
}

fn run_up(
    f: &WorkspaceFixture,
    log: &std::path::Path,
    admin: &str,
    https_port: u16,
    published: &str,
) -> std::process::Output {
    f.run_with_env(
        &["up", "feat/x"],
        &[
            ("WS_CADDY_ADMIN", admin),
            ("WS_CADDY_MANAGE", "managed"),
            ("WS_HTTPS_PORT", &https_port.to_string()),
            // The boot path preflights the admin port too, and the default
            // :2019 is legitimately busy on a box running its own Caddy.
            ("WS_CADDY_ADMIN_PORT", &free_port().to_string()),
            ("RT_LOG", log.to_str().unwrap()),
            ("RT_PUBLISHED", &format!("{published}:{https_port}")),
        ],
    )
}

const CATCH_ALL_ONLY: &str = r#"[{"@id":"ws-stopped-stack","handle":[]}]"#;
const ONE_LIVE_ROUTE: &str = r#"[
  {"@id":"ws-stopped-stack","handle":[]},
  {"@id":"ws-feat-y-api-web","match":[{"host":["feat-y.web.myapp.test"]}]}
]"#;

// The regression that motivated the guard: a local workspace always wants
// loopback, an offloaded workspace's Caddy publishes on 0.0.0.0 — which serves
// loopback already. Narrowing it took the offloaded workspace down for nothing.
#[test]
fn a_wider_publish_serves_a_local_workspace_untouched() {
    let (f, log) = fixture_with_fake_runtime("0.0.0.0");
    let port = free_port();
    let admin = admin_stub(port, ONE_LIVE_ROUTE);

    let out = run_up(&f, &log, &admin, port, "0.0.0.0");

    assert!(out.status.success(), "up must succeed: {}", stderr(&out));
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(!calls.contains("rm -f ws-caddy"), "must not remove the running Caddy:\n{calls}");
    assert!(!calls.contains("volume rm"), "must never touch the config volume:\n{calls}");
}

// An address the live publish can't serve us around, with someone's routes
// live: refuse, name the routes, destroy nothing.
#[test]
fn an_address_conflict_with_live_routes_refuses_teardown() {
    let (f, log) = fixture_with_fake_runtime("10.9.9.9");
    let port = free_port();
    let admin = admin_stub(port, ONE_LIVE_ROUTE);

    let out = run_up(&f, &log, &admin, port, "10.9.9.9");

    let err = stderr(&out);
    assert!(err.contains("feat-y.web.myapp.test"), "the refusal must name what's live:\n{err}");
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(!calls.contains("rm -f ws-caddy"), "a refusal must not tear down:\n{calls}");
    assert!(!calls.contains("volume rm"), "a refusal must not touch volumes:\n{calls}");
}

// With nothing live beyond the catch-all there is no one to protect: the
// address change applies by recreating the container — and only the container,
// never the config volume, so routes reload on the next boot.
#[test]
fn an_address_change_with_no_live_routes_recreates() {
    let (f, log) = fixture_with_fake_runtime("10.9.9.9");
    let port = free_port();
    let admin = admin_stub(port, CATCH_ALL_ONLY);

    let out = run_up(&f, &log, &admin, port, "10.9.9.9");

    assert!(out.status.success(), "up must succeed: {}", stderr(&out));
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(calls.contains("rm -f ws-caddy"), "the stale publish must be replaced:\n{calls}");
    assert!(!calls.contains("volume rm"), "an address move keeps the config volume:\n{calls}");
    assert!(calls.contains("run -d"), "a replacement Caddy must be started:\n{calls}");
}
