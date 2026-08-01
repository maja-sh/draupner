//! Slice 4 — ports + env + injection. End-to-end through `ws up` / `ws exec`:
//! host-port allocation (stable + collision-free), `WS_HOST_*` from the domain,
//! declared `env` substitution, and template rendering into a gitignored dest.

mod common;

use common::{stderr, WorkspaceFixture};
use std::process::Command;

/// The digits inside `PORT=[…]` emitted by a test's `up` script.
fn port_in(haystack: &str) -> u16 {
    common::port_after(haystack, "PORT=[")
}

#[test]
fn ws_port_is_allocated_and_injected() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(
        r#"{"repos":[{"name":"api",
            "services":{"mysql":{"port":3306}},
            "scripts":{"up":"echo PORT=[$WS_PORT_mysql]"}}]}"#,
    );
    f.run_ok(&["new", "api", "feat/foo"]);

    let out = f.run_ok(&["up", "feat/foo"]);
    let err = stderr(&out);
    let port = port_in(&err);
    assert!((20000..40000).contains(&port), "port {port} out of range:\n{err}");
    assert!(
        f.path().join(".ws-meta/feat-foo-api.json").is_file(),
        "expected .ws-meta record"
    );
}

#[test]
fn ws_port_is_stable_across_up_runs() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(
        r#"{"repos":[{"name":"api",
            "services":{"mysql":{"port":3306}},
            "scripts":{"up":"echo PORT=[$WS_PORT_mysql]"}}]}"#,
    );
    f.run_ok(&["new", "api", "feat/foo"]);

    let first = port_in(&stderr(&f.run_ok(&["up", "feat/foo"])));
    let second = port_in(&stderr(&f.run_ok(&["up", "feat/foo"])));
    assert_eq!(first, second, "a service must keep its port across `ws up` runs");
}

#[test]
fn distinct_branches_get_distinct_ports() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(
        r#"{"repos":[{"name":"api",
            "services":{"mysql":{"port":3306}},
            "scripts":{"up":"echo PORT=[$WS_PORT_mysql]"}}]}"#,
    );
    f.run_ok(&["new", "api", "feat/a"]);
    f.run_ok(&["new", "api", "feat/b"]);

    let a = port_in(&stderr(&f.run_ok(&["up", "feat/a"])));
    let b = port_in(&stderr(&f.run_ok(&["up", "feat/b"])));
    assert_ne!(a, b, "parallel worktrees must not collide on a host port");
}

#[test]
fn ws_host_is_injected_from_domain() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(
        r#"{"domain":"myapp.test","repos":[{"name":"api",
            "services":{"app":{"http":"api","port":80}},
            "scripts":{"up":"echo HOST=$WS_HOST_app"}}]}"#,
    );
    f.run_ok(&["new", "api", "feat/foo"]);

    let err = stderr(&f.run_ok(&["up", "feat/foo"]));
    assert!(
        err.contains("HOST=feat-foo.api.myapp.test"),
        "expected routed hostname in:\n{err}"
    );
}

#[test]
fn declared_env_mapping_is_substituted() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(
        r#"{"domain":"myapp.test","repos":[{"name":"api",
            "services":{"app":{"http":"api","port":80}},
            "env":{"APP_URL":"https://${WS_HOST_app}"},
            "scripts":{"up":"echo URL=$APP_URL"}}]}"#,
    );
    f.run_ok(&["new", "api", "feat/foo"]);

    let err = stderr(&f.run_ok(&["up", "feat/foo"]));
    assert!(
        err.contains("URL=https://feat-foo.api.myapp.test"),
        "declared env mapping should be substituted:\n{err}"
    );
}

#[test]
fn workspace_context_is_substitutable_in_declared_env() {
    // A project shouldn't have to learn ws's vocabulary to be driven by it. A
    // tool that shells out to a container runtime reads its own var; mapping that
    // var onto ws's answer belongs in `.ws.json`, not in the project's code.
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(
        r#"{"runtime":"podman","repos":[{"name":"api",
            "env":{"DOCKER_COMMAND":"${WS_RUNTIME}","MODE":"remote=${WS_REMOTE}"},
            "scripts":{"up":"echo CMD=$DOCKER_COMMAND MODE=$MODE"}}]}"#,
    );
    f.run_ok(&["new", "api", "feat/foo"]);

    let err = stderr(&f.run_ok(&["up", "feat/foo"]));
    assert!(
        err.contains("CMD=podman"),
        "WS_RUNTIME must substitute into a declared env value:\n{err}"
    );
    assert!(
        err.contains("MODE=remote=0"),
        "WS_REMOTE must substitute too, and mid-string:\n{err}"
    );
}

#[test]
fn template_is_rendered_into_gitignored_dest() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    // Commit a template on `main` so the worktree carries it. It embeds a WS_
    // token plus an nginx-style `$host` that must survive untouched.
    std::fs::write(
        f.path().join("api/svc.conf.tmpl"),
        "port ${WS_PORT_mysql};\nserver_name $host;\n",
    )
    .unwrap();
    f.git("api", &["add", "svc.conf.tmpl"]);
    f.git("api", &["commit", "-q", "-m", "add template"]);

    f.write_ws_json(
        r#"{"repos":[{"name":"api",
            "services":{"mysql":{"port":3306}},
            "templates":[{"src":"svc.conf.tmpl","dest":"svc.conf"}],
            "scripts":{"up":"echo PORT=[$WS_PORT_mysql]"}}]}"#,
    );
    f.run_ok(&["new", "api", "feat/foo"]);
    let port = port_in(&stderr(&f.run_ok(&["up", "feat/foo"])));

    let wt = f.path().join(".worktrees/feat-foo/api");
    let rendered = std::fs::read_to_string(wt.join("svc.conf")).expect("rendered dest");
    assert!(rendered.contains(&format!("port {port};")), "WS_PORT should render:\n{rendered}");
    assert!(rendered.contains("server_name $host;"), "$host must be untouched:\n{rendered}");

    // The rendered dest is git-invisible (info/exclude) — the worktree stays clean.
    let status = git_status(&wt);
    assert!(
        !status.contains("svc.conf"),
        "rendered dest must not show as untracked:\n{status}"
    );
}

#[test]
fn tracked_template_dest_is_refused() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    // Both the template AND its dest are committed — rendering would dirty a
    // tracked file, so `ws up` must refuse rather than clobber it.
    std::fs::write(f.path().join("api/svc.conf.tmpl"), "port ${WS_PORT_mysql};\n").unwrap();
    std::fs::write(f.path().join("api/svc.conf"), "port 3306;\n").unwrap();
    f.git("api", &["add", "svc.conf.tmpl", "svc.conf"]);
    f.git("api", &["commit", "-q", "-m", "add template + tracked dest"]);

    f.write_ws_json(
        r#"{"repos":[{"name":"api",
            "services":{"mysql":{"port":3306}},
            "templates":[{"src":"svc.conf.tmpl","dest":"svc.conf"}],
            "scripts":{"up":"echo UP"}}]}"#,
    );

    // Templates render before post-create, so the refusal surfaces at `ws new`.
    let out = f.run(&["new", "api", "feat/foo"]);
    assert_eq!(out.status.code(), Some(1), "rendering onto a tracked dest must fail");
    let err = stderr(&out);
    assert!(err.contains("tracked by git"), "expected the tracked-dest error:\n{err}");
}

#[test]
fn exec_local_gets_ws_port_env() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(
        r#"{"repos":[{"name":"api","services":{"mysql":{"port":3306}}}]}"#,
    );
    f.run_ok(&["new", "api", "feat/foo"]);

    let out = f.run_in(
        ".worktrees/feat-foo/api",
        &["exec", "api", "--", "sh", "-c", "echo PORT=[$WS_PORT_mysql]"],
    );
    assert!(out.status.success(), "exec failed:\n{}", stderr(&out));
    let combined = format!("{}{}", common::stdout(&out), stderr(&out));
    let port = port_in(&combined);
    assert!((20000..40000).contains(&port), "exec should inject WS_PORT: {port}");
}

/// `git status --porcelain` in a worktree, with the isolated git env the
/// fixture uses elsewhere.
fn git_status(wt: &std::path::Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(wt)
        .args(["status", "--porcelain"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git status");
    String::from_utf8_lossy(&out.stdout).into_owned()
}
