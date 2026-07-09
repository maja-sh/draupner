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
    // Both step lines resolved ok (non-tty renders "ok").
    assert_eq!(err.matches("up hook").count(), 2, "expected two up-hook steps in:\n{err}");
}

#[test]
fn partial_only_repos_with_worktrees_run() {
    let f = WorkspaceFixture::with_repos(&["api", "app"]);
    f.add_hook("api", "up", "echo API-UP-RAN");
    f.add_hook("app", "up", "echo APP-UP-RAN");
    // Only api gets a worktree for this branch.
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
    // No up hook added — repo has a worktree but nothing to run.
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

    // First repo still ran to completion despite the second failing.
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
    // Second run must not fail — nothing ws adds should break hook idempotency.
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
    // Hook lines carry the `<repo>-<event>` tag.
    assert!(err.contains("api-up"), "expected hook tag in:\n{err}");
}

// A repo with a declared HTTP service + a top-level `domain` routes through
// Caddy. Since 6.2b the override `!reset`s an HTTP service's host publish, so it
// is reachable ONLY via its Caddy route — an install that fails means the
// running stack is unreachable, so `ws up` must fail (not report success). Uses
// a bare declared-HTTP repo (no compose file needed) and a dead admin endpoint.
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

    // Dead admin endpoint → route_up can't install the route.
    let out = f.run_with_env(&["up", "feat/route"], &[("WS_CADDY_ADMIN", "http://127.0.0.1:1")]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "an unroutable HTTP service must fail `up`, not report success:\n{}",
        stderr(&out)
    );
    // The hook itself ran — the failure is the routing step, not the hook.
    assert!(stderr(&out).contains("up-ran"), "the up hook should still have run:\n{}", stderr(&out));
}

// The mirror of the above: with a reachable admin (the fixture's stub) the same
// stack comes up green and reports its routed hostname.
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

    let err = stderr(&f.run_ok(&["up", "feat/route"]));
    assert!(err.contains("feat-route.web.myapp.test"), "expected the routed hostname in:\n{err}");
}

// A raw-TCP-only stack in a routing-active workspace needs neither Caddy nor
// `ws-net` (no HTTP service ⇒ no route, no host publish dropped), so a route
// call is never made and `up` succeeds regardless of Caddy.
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
    let out = f.run_with_env(&["up", "feat/db"], &[("WS_CADDY_ADMIN", "http://127.0.0.1:1")]);
    assert!(
        out.status.success(),
        "a raw-TCP-only stack must not need Caddy:\n{}",
        stderr(&out)
    );
    assert!(!stderr(&out).contains("routed"), "no route should be attempted:\n{}", stderr(&out));
}
