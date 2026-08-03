//! Compose-driven `services` auto-derivation, end-to-end through `ws up`. A CI
//! host without `docker compose` skips these rather than failing.

mod common;

use common::{port_after, stderr, WorkspaceFixture};
use std::process::Command;

/// Derivation only parses the compose file — no daemon needed, but the CLI must
/// exist.
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
    // One of each shape the derivation distinguishes: HTTP (web), single-port
    // raw-TCP (db), and multi-port (mailpit).
    commit_compose(
        &f,
        "services:\n\
        \x20 web:\n    image: nginx\n    ports: [\"8080:80\"]\n\
        \x20 db:\n    image: mysql:8\n    ports: [\"3306\"]\n\
        \x20 mailpit:\n    image: axllent/mailpit\n    ports: [\"1025:1025\", \"8025:8025\"]\n",
    );

    // No `services` declared — everything comes from the live derivation.
    f.write_ws_json(
        r#"{"domain":"myapp.test","runtime":"docker","repos":[{"name":"api",
            "scripts":{"up":"echo HOST=$WS_HOST_web DB=[$WS_PORT_db] MP=[$WS_PORT_mailpit_8025]"}}]}"#,
    );
    f.run_ok(&["new", "api", "feat/foo"]);

    let err = stderr(&f.run_ok(&["up", "feat/foo"]));
    assert!(err.contains("HOST=feat-foo.web.myapp.test"), "derived HTTP host missing:\n{err}");
    let db = port_after(&err, "DB=[");
    assert!((20000..40000).contains(&db), "db port out of range ({db}):\n{err}");
    // A multi-port service derives one key per port, suffixed with the number.
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
    commit_compose(
        &f,
        "services:\n  mailpit:\n    image: axllent/mailpit\n    ports: [\"8025:8025\"]\n",
    );

    // Derivation would make mailpit raw-TCP; the override routes it as HTTP.
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
    commit_compose(
        &f,
        "services:\n  web:\n    image: nginx\n    ports: [\"8080:80\"]\n\
        \x20 db:\n    image: mysql:8\n    ports: [\"3306\"]\n",
    );

    // `ws exec` is a read-only phase and never derives, so it can only resolve
    // these from the effective set `up` recorded.
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
    // A failed derivation must surface loudly rather than bring up an unrouted
    // stack behind a dim note.
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(
        r#"{"runtime":"ws-no-such-runtime","repos":[{"name":"api","kind":"container",
            "scripts":{"up":"echo UP"}}]}"#,
    );

    // Derivation runs at `ws new`, since post-create is an activating event.
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
    write_compose(
        &f,
        "services:\n  web:\n    image: nginx\n    ports: [\"8080:80\"]\n\
        \x20 db:\n    image: mysql:8\n    ports: [\"3306\"]\n",
    );

    let out = f.run(&["init", "--runtime", "docker"]);
    assert!(out.status.success(), "init failed:\n{}", stderr(&out));

    let ws_json = std::fs::read_to_string(f.path().join(".ws.json")).unwrap();
    assert!(ws_json.contains("\"runtime\": \"docker\""), "runtime should be docker:\n{ws_json}");
    assert!(ws_json.contains("\"services\""), "expected a scaffolded services block:\n{ws_json}");
    assert!(ws_json.contains("\"web\""), "expected the web service:\n{ws_json}");
    assert!(ws_json.contains("\"db\""), "expected the db service:\n{ws_json}");
}

#[test]
fn container_override_deconflicts_raw_tcp_ports() {
    if !compose_available() {
        eprintln!("skipping: `docker compose` not available");
        return;
    }

    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    commit_compose(
        &f,
        "services:\n\
        \x20 web:\n    image: nginx\n    ports: [\"8080:80\"]\n\
        \x20 db:\n    image: mysql:8\n    ports: [\"3306:3306\"]\n",
    );

    // `config -q` proves the generated `!override` actually parses on this
    // compose version rather than only looking right.
    f.write_ws_json(
        r#"{"domain":"myapp.test","runtime":"docker","repos":[{"name":"api",
            "scripts":{"up":"docker compose config -q && echo PROJ=$COMPOSE_PROJECT_NAME"}}]}"#,
    );

    f.run_ok(&["new", "api", "feat/a"]);
    f.run_ok(&["new", "api", "feat/b"]);
    let err_a = stderr(&f.run_ok(&["up", "feat/a"]));
    let err_b = stderr(&f.run_ok(&["up", "feat/b"]));

    assert!(err_a.contains("PROJ=feat-a-api"), "project name not namespaced:\n{err_a}");
    assert!(err_b.contains("PROJ=feat-b-api"), "project name not namespaced:\n{err_b}");

    // An HTTP service drops its host publish entirely (`!reset []`) — it is
    // reachable only through Caddy on `ws-net`.
    let ov_a = read_override(&f, "feat-a-api");
    let ov_b = read_override(&f, "feat-b-api");
    assert!(ov_a.contains("db:") && ov_a.contains("ports: !override"), "no db override:\n{ov_a}");
    assert!(ov_a.contains("ws-net"), "HTTP web should join ws-net:\n{ov_a}");
    assert!(ov_a.contains("web:\n    ports: !reset []"), "6.2b must !reset the HTTP publish:\n{ov_a}");
    let a = override_port(&ov_a);
    let b = override_port(&ov_b);
    assert!((20000..40000).contains(&a), "db host port out of range ({a}):\n{ov_a}");
    assert_ne!(a, b, "parallel worktrees must reissue db on distinct host ports");
}

#[test]
fn a_bare_name_compose_repo_is_namespaced_per_worktree() {
    if !compose_available() {
        eprintln!("skipping: `docker compose` not available");
        return;
    }

    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    commit_compose(&f, "services:\n  db:\n    image: mysql:8\n    ports: [\"3306:3306\"]\n");

    // A bare-name entry still infers a container repo from the compose file, so it
    // needs namespacing too — otherwise every worktree's stack is compose's
    // default project `api` and they collide.
    f.write_ws_json(r#"{"domain":"myapp.test","runtime":"docker","repos":["api"]}"#);
    // Stands in for the default `docker compose up -d` — same resolution path,
    // same env — so the test starts no containers.
    f.add_hook("api", "up", "echo PROJ=[$COMPOSE_PROJECT_NAME]");

    f.run_ok(&["new", "api", "feat/a"]);
    f.run_ok(&["new", "api", "feat/b"]);
    let err_a = stderr(&f.run_ok(&["up", "feat/a"]));
    let err_b = stderr(&f.run_ok(&["up", "feat/b"]));

    assert!(err_a.contains("PROJ=[feat-a-api]"), "bare-name project not namespaced:\n{err_a}");
    assert!(err_b.contains("PROJ=[feat-b-api]"), "bare-name project not namespaced:\n{err_b}");

    let a = override_port(&read_override(&f, "feat-a-api"));
    let b = override_port(&read_override(&f, "feat-b-api"));
    assert!((20000..40000).contains(&a), "db host port out of range ({a})");
    assert_ne!(a, b, "parallel worktrees must reissue db on distinct host ports");
}

#[test]
fn container_override_injects_declared_env() {
    if !compose_available() {
        eprintln!("skipping: `docker compose` not available");
        return;
    }

    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    commit_compose(&f, "services:\n  web:\n    image: nginx\n    ports: [\"8080:80\"]\n");

    f.write_ws_json(
        r#"{"domain":"myapp.test","runtime":"docker","repos":[{"name":"api",
            "env":{"APP_URL":"https://${WS_HOST_web}"},
            "scripts":{"up":"docker compose config -q && echo OK"}}]}"#,
    );
    f.run_ok(&["new", "api", "feat/a"]);
    assert!(stderr(&f.run_ok(&["up", "feat/a"])).contains("OK"), "merged config should validate");

    let ov = read_override(&f, "feat-a-api");
    assert!(
        ov.contains("APP_URL: \"https://feat-a.web.myapp.test\""),
        "declared env not materialized into the override:\n{ov}"
    );
}

fn read_override(f: &WorkspaceFixture, stem: &str) -> String {
    std::fs::read_to_string(f.path().join(".ws-meta").join(format!("{stem}.override.yml")))
        .unwrap_or_else(|e| panic!("read override for {stem}: {e}"))
}

/// The host half of the first `"<host>:<internal>"` mapping in an override.
fn override_port(ov: &str) -> u16 {
    let line = ov.lines().find(|l| l.trim_start().starts_with("- \"")).expect("a ports mapping");
    let inside = line.trim().trim_start_matches("- \"").trim_end_matches('"');
    inside.split(':').next().unwrap().parse().unwrap_or(0)
}

fn write_compose(f: &WorkspaceFixture, yaml: &str) {
    std::fs::write(f.path().join("api/docker-compose.yml"), yaml).unwrap();
}

/// Committed, so a new worktree carries the compose file.
fn commit_compose(f: &WorkspaceFixture, yaml: &str) {
    write_compose(f, yaml);
    f.git("api", &["add", "docker-compose.yml"]);
    f.git("api", &["commit", "-q", "-m", "add compose"]);
}
