mod common;

use common::{stderr, WorkspaceFixture};

#[test]
fn happy_path_both_hooks_run() {
    let f = WorkspaceFixture::with_repos(&["api", "app"]);
    f.add_hook("api", "up", "echo API-UP-RAN");
    f.add_hook("app", "up", "echo APP-UP-RAN");
    f.run_ok(&["new", "api,app", "feat/foo"]);

    let out = f.run_ok(&["up", "feat/foo"]);
    let err = stderr(&out);

    assert!(err.contains("API-UP-RAN"), "expected api hook output in:\n{err}");
    assert!(err.contains("APP-UP-RAN"), "expected app hook output in:\n{err}");
    assert_eq!(err.matches("up hook").count(), 2, "expected two up-hook steps in:\n{err}");
}

#[test]
fn prepare_hook_runs_before_up() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.add_hook("api", "prepare", "echo PREPARE-RAN");
    f.add_hook("api", "up", "echo UP-RAN");
    f.run_ok(&["new", "api", "feat/foo"]);

    let out = f.run_ok(&["up", "feat/foo"]);
    let err = stderr(&out);

    assert!(err.contains("prepare hook"), "a prepare step line should show:\n{err}");
    let p = err.find("PREPARE-RAN").expect("prepare ran");
    let u = err.find("UP-RAN").expect("up ran");
    assert!(p < u, "prepare must run before up:\n{err}");
}

#[test]
fn prepare_is_opt_in_no_step_when_absent() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.add_hook("api", "up", "echo UP-RAN");
    f.run_ok(&["new", "api", "feat/foo"]);

    let out = f.run_ok(&["up", "feat/foo"]);
    assert!(
        !stderr(&out).contains("prepare hook"),
        "prepare is opt-in — no step when there's no prepare hook"
    );
}

#[test]
fn a_failed_prepare_skips_up() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.add_hook("api", "prepare", "echo staging; exit 1");
    f.add_hook("api", "up", "echo UP-RAN");
    f.run_ok(&["new", "api", "feat/foo"]);

    let out = f.run(&["up", "feat/foo"]);
    let err = stderr(&out);
    assert!(!out.status.success(), "a failed prepare must fail the up:\n{err}");
    assert!(!err.contains("UP-RAN"), "up must be skipped when prepare fails:\n{err}");
}

#[test]
fn partial_only_repos_with_worktrees_run() {
    let f = WorkspaceFixture::with_repos(&["api", "app"]);
    f.add_hook("api", "up", "echo API-UP-RAN");
    f.add_hook("app", "up", "echo APP-UP-RAN");
    f.run_ok(&["new", "api", "feat/partial"]);

    let out = f.run_ok(&["up", "feat/partial"]);
    let err = stderr(&out);

    assert!(err.contains("API-UP-RAN"), "expected api hook output in:\n{err}");
    assert!(!err.contains("APP-UP-RAN"), "app has no worktree; its hook must not run:\n{err}");
}

#[test]
fn no_worktrees_is_preflight_error() {
    let f = WorkspaceFixture::with_repos(&["api"]);

    let out = f.run(&["up", "feat/none"]);
    assert_eq!(out.status.code(), Some(2), "expected exit 2 when no worktrees");
    let err = stderr(&out);
    assert!(err.contains("no worktrees"), "expected 'no worktrees' in:\n{err}");
}

#[test]
fn missing_hook_is_skipped_not_an_error() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/nohook"]);

    let out = f.run_ok(&["up", "feat/nohook"]);
    let err = stderr(&out);
    assert!(err.contains("no up hook"), "expected dim skip line in:\n{err}");
}

#[test]
fn failure_isolation_second_repo_hook_fails() {
    let f = WorkspaceFixture::with_repos(&["api", "app"]);
    f.add_hook("api", "up", "echo API-UP-RAN");
    f.add_hook("app", "up", "echo APP-UP-TRIED; exit 1");
    f.run_ok(&["new", "api,app", "feat/iso"]);

    let out = f.run(&["up", "feat/iso"]);
    assert_eq!(out.status.code(), Some(1), "expected exit 1 when a hook fails");
    let err = stderr(&out);

    assert!(err.contains("API-UP-RAN"), "first repo should still run:\n{err}");
    assert!(err.contains("APP-UP-TRIED"), "second repo hook should have been attempted:\n{err}");
    assert!(err.contains("FAIL"), "expected a failed step line in:\n{err}");
    assert!(err.contains("app"), "failure summary should name the repo in:\n{err}");
}

#[test]
fn idempotent_second_run_also_succeeds() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.add_hook("api", "up", "echo API-UP-RAN");
    f.run_ok(&["new", "api", "feat/idem"]);

    f.run_ok(&["up", "feat/idem"]);
    // Nothing drup adds around the hook should break idempotency.
    let out = f.run_ok(&["up", "feat/idem"]);
    assert!(stderr(&out).contains("API-UP-RAN"));
}

#[test]
fn hook_streaming_stdout_and_stderr() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.add_hook(
        "api",
        "up",
        "echo out1; echo out2; echo err1 >&2; echo out3",
    );
    f.run_ok(&["new", "api", "feat/stream"]);

    let out = f.run_ok(&["up", "feat/stream"]);
    let err = stderr(&out);

    for marker in ["out1", "out2", "out3", "err1"] {
        assert!(err.contains(marker), "expected {marker} in streamed output:\n{err}");
    }
    assert!(err.contains("api-up"), "expected hook tag in:\n{err}");
}

// Since 6.2b an HTTP service with a `domain` has its host publish `!reset`, so an unroutable stack is unreachable and `up` must fail rather than report success.
#[test]
fn up_fails_when_an_http_route_cannot_be_installed() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(
        r#"{"domain":"myapp.test","repos":[{"name":"api",
            "services":{"web":{"http":true,"port":80}},
            "scripts":{"up":"echo up-ran"}}]}"#,
    );
    f.run_ok(&["new", "api", "feat/route"]);

    // Dead admin endpoint so route_up cannot install the route.
    let out = f.run_with_env(&["up", "feat/route"], &[("DRUP_CADDY_ADMIN", "http://127.0.0.1:1")]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "an unroutable HTTP service must fail `up`, not report success:\n{}",
        stderr(&out)
    );
    // Guards that the failure is the routing step, not the hook itself.
    assert!(stderr(&out).contains("up-ran"), "the up hook should still have run:\n{}", stderr(&out));
}

#[test]
fn up_succeeds_and_routes_when_caddy_is_reachable() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(
        r#"{"domain":"myapp.test","repos":[{"name":"api",
            "services":{"web":{"http":true,"port":80}},
            "scripts":{"up":"echo up-ran"}}]}"#,
    );
    f.run_ok(&["new", "api", "feat/route"]);

    // Pinned so this doesn't depend on the machine's opted-in HTTPS port.
    let out = f.run_with_env(&["up", "feat/route"], &[("DRUP_HTTPS_PORT", "8443")]);
    let err = stderr(&out);
    assert!(out.status.success(), "up failed:\n{err}");
    assert!(
        err.contains("feat-route.web.myapp.test:8443"),
        "the routed line must carry the port:\n{err}"
    );
}

// A raw-TCP-only stack installs no route and keeps its host publish, so `up` must succeed regardless of whether Caddy is reachable.
#[test]
fn raw_tcp_only_up_makes_no_route_call() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(
        r#"{"domain":"myapp.test","repos":[{"name":"api",
            "services":{"db":{"port":3306}},
            "scripts":{"up":"echo up-ran"}}]}"#,
    );
    f.run_ok(&["new", "api", "feat/db"]);

    // Even with a dead admin, a stack that installs no route must still come up.
    let out = f.run_with_env(&["up", "feat/db"], &[("DRUP_CADDY_ADMIN", "http://127.0.0.1:1")]);
    assert!(
        out.status.success(),
        "a raw-TCP-only stack must not need Caddy:\n{}",
        stderr(&out)
    );
    assert!(!stderr(&out).contains("routed"), "no route should be attempted:\n{}", stderr(&out));
}
