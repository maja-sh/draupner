mod common;

use common::{stderr, WorkspaceFixture};
use std::process::Command;

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
    // So a project can be driven by ws without learning its vocabulary: mapping
    // ws's answer onto the var a tool already reads belongs in `.ws.json`.
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
    // The nginx-style `$host` must survive untouched — substitution is brace-only
    // precisely so a real config's own variables aren't eaten.
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
    // Both the template and its dest are committed, so rendering would clobber a
    // tracked file.
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
fn template_dest_escaping_the_worktree_is_refused() {
    // `.ws.json` arrives with a clone, so a hostile `templates` entry can write
    // attacker-chosen bytes anywhere the user can, declaring no `scripts` at all.
    // Both spellings are covered: `..` walks out, and an absolute `dest` discards
    // the worktree entirely in `Path::join`.
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    std::fs::write(f.path().join("api/payload.tmpl"), "pwned\n").unwrap();
    f.git("api", &["add", "payload.tmpl"]);
    f.git("api", &["commit", "-q", "-m", "add payload"]);

    f.write_ws_json(
        r#"{"repos":[{"name":"api",
            "templates":[{"src":"payload.tmpl","dest":"../../../pwned.txt"}],
            "scripts":{"up":"echo UP"}}]}"#,
    );
    let out = f.run(&["new", "api", "feat/foo"]);
    assert_eq!(out.status.code(), Some(1), "a traversing template dest must fail");
    let err = stderr(&out);
    assert!(err.contains("must not contain `..`"), "expected the traversal refusal:\n{err}");
    assert!(!f.path().join("pwned.txt").exists(), "the escaping write must not happen");

    let abs = f.path().join("abs-pwned.txt");
    f.write_ws_json(&format!(
        r#"{{"repos":[{{"name":"api",
            "templates":[{{"src":"payload.tmpl","dest":"{}"}}],
            "scripts":{{"up":"echo UP"}}}}]}}"#,
        abs.display()
    ));
    let out = f.run(&["new", "api", "feat/bar"]);
    assert_eq!(out.status.code(), Some(1), "an absolute template dest must fail");
    let err = stderr(&out);
    assert!(err.contains("must be a path inside the worktree"), "expected the absolute-path refusal:\n{err}");
    assert!(!abs.exists(), "the escaping write must not happen");
}

#[test]
fn template_src_escaping_the_worktree_is_refused() {
    // The read side of the same hole: an escaping `src` pulls an SSH key or a
    // dotfile into a dest the repo then syncs.
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    std::fs::write(f.path().join("secret.txt"), "top secret\n").unwrap();

    f.write_ws_json(
        r#"{"repos":[{"name":"api",
            "templates":[{"src":"../../../secret.txt","dest":"leak.txt"}],
            "scripts":{"up":"echo UP"}}]}"#,
    );
    let out = f.run(&["new", "api", "feat/foo"]);
    assert_eq!(out.status.code(), Some(1), "a traversing template src must fail");
    let err = stderr(&out);
    assert!(err.contains("must not contain `..`"), "expected the traversal refusal:\n{err}");
    assert!(
        !f.path().join(".worktrees/feat-foo/api/leak.txt").exists(),
        "nothing should have been read or written"
    );
}

#[test]
fn a_nested_template_dest_still_renders() {
    // The traversal guard must not cost the ordinary shape — a template rendered
    // into a subdirectory of the worktree.
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    std::fs::create_dir_all(f.path().join("api/deploy")).unwrap();
    std::fs::write(
        f.path().join("api/deploy/svc.conf.tmpl"),
        "port ${WS_PORT_mysql};\n",
    )
    .unwrap();
    f.git("api", &["add", "deploy/svc.conf.tmpl"]);
    f.git("api", &["commit", "-q", "-m", "add nested template"]);

    f.write_ws_json(
        r#"{"repos":[{"name":"api",
            "services":{"mysql":{"port":3306}},
            "templates":[{"src":"deploy/svc.conf.tmpl","dest":"config/generated/svc.conf"}],
            "scripts":{"up":"echo PORT=[$WS_PORT_mysql]"}}]}"#,
    );
    f.run_ok(&["new", "api", "feat/foo"]);
    let port = port_in(&stderr(&f.run_ok(&["up", "feat/foo"])));

    let rendered =
        std::fs::read_to_string(f.path().join(".worktrees/feat-foo/api/config/generated/svc.conf"))
            .expect("nested dest rendered");
    assert!(rendered.contains(&format!("port {port};")), "WS_PORT should render:\n{rendered}");
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
