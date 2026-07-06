//! Compose-driven `services` auto-derivation, end-to-end through `ws up`.
//! Gated on a working `docker compose` (present on the dev box; a CI host
//! without it skips rather than fails).

mod common;

use common::{stderr, WorkspaceFixture};
use std::process::Command;

/// Is `docker compose config` usable here? The derivation only parses the file
/// (no daemon needed), but the CLI must exist.
fn compose_available() -> bool {
    Command::new("docker")
        .args(["compose", "version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn services_are_derived_from_compose_config() {
    if !compose_available() {
        eprintln!("skipping: `docker compose` not available");
        return;
    }

    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    // A real compose file committed on `main`: an HTTP service (web on 80), a
    // single-port raw-TCP service (db), and a multi-port one (mailpit).
    std::fs::write(
        f.path().join("api/docker-compose.yml"),
        "services:\n\
        \x20 web:\n    image: nginx\n    ports: [\"8080:80\"]\n\
        \x20 db:\n    image: mysql:8\n    ports: [\"3306\"]\n\
        \x20 mailpit:\n    image: axllent/mailpit\n    ports: [\"1025:1025\", \"8025:8025\"]\n",
    )
    .unwrap();
    f.git("api", &["add", "docker-compose.yml"]);
    f.git("api", &["commit", "-q", "-m", "add compose"]);

    // No `services` declared — everything comes from the live derivation.
    f.write_ws_json(
        r#"{"domain":"myapp.test","runtime":"docker","repos":[{"name":"api",
            "scripts":{"up":"echo HOST=$WS_HOST_web DB=[$WS_PORT_db] MP=[$WS_PORT_mailpit_8025]"}}]}"#,
    );
    f.run_ok(&["new", "api", "feat/foo"]);

    let err = stderr(&f.run_ok(&["up", "feat/foo"]));
    // web (80) → HTTP-routed hostname.
    assert!(err.contains("HOST=feat-foo.web.myapp.test"), "derived HTTP host missing:\n{err}");
    // db (3306) → bare-name raw-TCP allocation.
    let db = port_after(&err, "DB=[");
    assert!((20000..40000).contains(&db), "db port out of range ({db}):\n{err}");
    // mailpit is multi-port → its UI port derives as `mailpit_8025`.
    let mp = port_after(&err, "MP=[");
    assert!((20000..40000).contains(&mp), "mailpit_8025 port missing ({mp}):\n{err}");
    assert_ne!(db, mp);
}

#[test]
fn declared_services_override_the_derivation() {
    if !compose_available() {
        eprintln!("skipping: `docker compose` not available");
        return;
    }

    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    std::fs::write(
        f.path().join("api/docker-compose.yml"),
        "services:\n  mailpit:\n    image: axllent/mailpit\n    ports: [\"8025:8025\"]\n",
    )
    .unwrap();
    f.git("api", &["add", "docker-compose.yml"]);
    f.git("api", &["commit", "-q", "-m", "add compose"]);

    // Derivation would make mailpit (8025) a raw-TCP service; the override routes
    // it as HTTP under the `mail` subdomain instead.
    f.write_ws_json(
        r#"{"domain":"myapp.test","runtime":"docker","repos":[{"name":"api",
            "services":{"mailpit":{"http":"mail","port":8025}},
            "scripts":{"up":"echo HOST=[$WS_HOST_mailpit] PORT=[$WS_PORT_mailpit]"}}]}"#,
    );
    f.run_ok(&["new", "api", "feat/foo"]);

    let err = stderr(&f.run_ok(&["up", "feat/foo"]));
    assert!(err.contains("HOST=[feat-foo.mail.myapp.test]"), "override should route as HTTP:\n{err}");
    assert!(err.contains("PORT=[]"), "overridden HTTP service should not get a WS_PORT:\n{err}");
}

#[test]
fn read_phases_resolve_derived_services_after_up() {
    if !compose_available() {
        eprintln!("skipping: `docker compose` not available");
        return;
    }

    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    std::fs::write(
        f.path().join("api/docker-compose.yml"),
        "services:\n  web:\n    image: nginx\n    ports: [\"8080:80\"]\n\
        \x20 db:\n    image: mysql:8\n    ports: [\"3306\"]\n",
    )
    .unwrap();
    f.git("api", &["add", "docker-compose.yml"]);
    f.git("api", &["commit", "-q", "-m", "add compose"]);

    // No declared services — `web`/`db` exist only via derivation. `up` records
    // the effective set so a later `ws exec` (a read-only, non-deriving phase)
    // can still resolve their host/port.
    f.write_ws_json(
        r#"{"domain":"myapp.test","runtime":"docker","repos":[{"name":"api",
            "scripts":{"up":"echo up"}}]}"#,
    );
    f.run_ok(&["new", "api", "feat/foo"]);
    f.run_ok(&["up", "feat/foo"]);

    let out = f.run_in(
        ".worktrees/feat-foo/api",
        &["exec", "api", "--", "sh", "-c", "echo H=$WS_HOST_web DB=[$WS_PORT_db]"],
    );
    assert!(out.status.success(), "exec failed:\n{}", stderr(&out));
    let combined = format!("{}{}", common::stdout(&out), stderr(&out));
    assert!(
        combined.contains("H=feat-foo.web.myapp.test"),
        "exec should resolve the derived host from the recorded service set:\n{combined}"
    );
    let db = port_after(&combined, "DB=[");
    assert!((20000..40000).contains(&db), "exec should resolve the derived port ({db}):\n{combined}");
}

#[test]
fn explicit_container_hard_fails_when_derivation_fails() {
    // No runtime needed: an explicit `kind: container` pointed at a bogus runtime
    // can't derive, and that must surface loudly rather than bring up an
    // unrouted stack behind a dim note.
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(
        r#"{"runtime":"ws-no-such-runtime","repos":[{"name":"api","kind":"container",
            "scripts":{"up":"echo UP"}}]}"#,
    );

    // Derivation runs at `ws new` (post-create is an activating event).
    let out = f.run(&["new", "api", "feat/foo"]);
    assert_eq!(out.status.code(), Some(1), "a broken container derivation must fail");
    let err = stderr(&out);
    assert!(err.contains("container repo"), "expected the container-derive error:\n{err}");
}

#[test]
fn init_scaffolds_derived_services() {
    if !compose_available() {
        eprintln!("skipping: `docker compose` not available");
        return;
    }

    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    std::fs::write(
        f.path().join("api/docker-compose.yml"),
        "services:\n  web:\n    image: nginx\n    ports: [\"8080:80\"]\n\
        \x20 db:\n    image: mysql:8\n    ports: [\"3306\"]\n",
    )
    .unwrap();

    // No .ws.json yet — init creates it, scaffolding the derived services.
    let out = f.run(&["init", "--runtime", "docker"]);
    assert!(out.status.success(), "init failed:\n{}", stderr(&out));

    let ws_json = std::fs::read_to_string(f.path().join(".ws.json")).unwrap();
    assert!(ws_json.contains("\"runtime\": \"docker\""), "runtime should be docker:\n{ws_json}");
    assert!(ws_json.contains("\"services\""), "expected a scaffolded services block:\n{ws_json}");
    assert!(ws_json.contains("\"web\""), "expected the web service:\n{ws_json}");
    assert!(ws_json.contains("\"db\""), "expected the db service:\n{ws_json}");
}

/// Parse the digits in `<marker>NNNNN]`.
fn port_after(haystack: &str, marker: &str) -> u16 {
    let start = haystack.find(marker).unwrap_or_else(|| panic!("{marker} in output")) + marker.len();
    let end = start + haystack[start..].find(']').expect("closing ]");
    haystack[start..end].parse().unwrap_or(0)
}
