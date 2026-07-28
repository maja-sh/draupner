//! `kind: host` repos are supervised as a detached, `ws`-owned process tracked by
//! a PID file (no systemd/launchd — works on macOS too). These run the *real*
//! mechanism: `ws up` launches a `sleep`, and we assert liveness via the pid file,
//! then `ws down` stops it. `$HOME` is redirected into the fixture, so `~/.ws-run`
//! is isolated.

mod common;

use common::{stderr, WorkspaceFixture};
use std::path::Path;

/// A host repo whose `up` records the injected env, then runs a long sleep as the
/// supervised (tracked) process.
fn host_fixture() -> WorkspaceFixture {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(
        r#"{"domain":"myapp.test","repos":[{"name":"api","kind":"host",
            "services":{"web":{"http":"web","port":8080}},
            "scripts":{"up":"printf %s \"$WS_HOST_web\" > env-marker; exec sleep 40"}}]}"#,
    );
    f
}

/// Whether the host process for `feat/foo`'s api is alive, read the same way `ws`
/// does: the pid file under the fixture HOME, then `kill -0`.
fn proc_alive(f: &WorkspaceFixture) -> bool {
    let pid_file = f.path().join(".ws-run/ws-feat-foo-api.pid");
    let Ok(contents) = std::fs::read_to_string(&pid_file) else { return false };
    // The pid file is two lines — pid, then the start-time identity. We only need
    // the pid to probe liveness the way the shell does.
    let pid = contents.lines().next().unwrap_or("").trim();
    !pid.is_empty()
        && std::process::Command::new("kill")
            .args(["-0", pid])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
}

fn stop(f: &WorkspaceFixture) {
    // Always tear the worktree down so no detached `sleep` leaks past the test.
    let _ = f.run(&["rm", "feat/foo", "--force"]);
}

#[test]
fn host_up_launches_a_detached_process_with_injected_env() {
    let f = host_fixture();
    f.run_ok(&["new", "api", "feat/foo"]);

    let out = f.run_ok(&["up", "feat/foo"]);
    assert!(out.status.success(), "up failed:\n{}", stderr(&out));

    // The process is alive (pid file + kill -0)…
    assert!(proc_alive(&f), "host process should be running after up");
    // …the injected routing env reached the command…
    let marker = f.path().join(".worktrees/feat-foo/api/env-marker");
    let got = std::fs::read_to_string(&marker).unwrap_or_default();
    assert_eq!(got, "feat-foo.web.myapp.test", "WS_HOST_web not injected");

    stop(&f);
    assert!(!proc_alive(&f), "host process should be gone after rm");
}

#[test]
fn host_down_stops_the_process() {
    let f = host_fixture();
    f.run_ok(&["new", "api", "feat/foo"]);
    f.run_ok(&["up", "feat/foo"]);
    assert!(proc_alive(&f), "should be up before down");

    let out = f.run_ok(&["down", "feat/foo"]);
    assert!(out.status.success(), "down failed:\n{}", stderr(&out));
    assert!(!proc_alive(&f), "host process should be stopped after down");
    stop(&f);
}

#[test]
fn host_up_is_idempotent_when_already_running() {
    let f = host_fixture();
    f.run_ok(&["new", "api", "feat/foo"]);
    f.run_ok(&["up", "feat/foo"]);
    let pid1 = std::fs::read_to_string(f.path().join(".ws-run/ws-feat-foo-api.pid")).unwrap();

    // A second up must NOT relaunch — same pid, and it says "already running".
    let out = f.run_ok(&["up", "feat/foo"]);
    let pid2 = std::fs::read_to_string(f.path().join(".ws-run/ws-feat-foo-api.pid")).unwrap();
    assert_eq!(pid1, pid2, "up must not relaunch a running host process");
    assert!(
        stderr(&out).contains("already running"),
        "expected an already-running note:\n{}",
        stderr(&out)
    );
    stop(&f);
}

#[test]
fn host_up_with_no_up_script_is_skipped() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(
        r#"{"repos":[{"name":"api","kind":"host","services":{"web":{"http":"web","port":8080}}}]}"#,
    );
    f.run_ok(&["new", "api", "feat/foo"]);

    let out = f.run_ok(&["up", "feat/foo"]);
    assert!(out.status.success(), "up failed:\n{}", stderr(&out));
    assert!(stderr(&out).contains("no up script"), "expected a skip note:\n{}", stderr(&out));
    assert!(
        !Path::new(&f.path().join(".ws-run/ws-feat-foo-api.pid")).exists(),
        "nothing to launch → no pid file"
    );
}
