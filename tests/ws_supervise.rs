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
            "scripts":{"up":"printf '%s %s' \"$WS_HOST_web\" \"$WS_BRANCH_SANITIZED\" > env-marker; exec sleep 40"}}]}"#,
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
    // …both the injected routing env (WS_HOST_web) AND the base contract
    // (WS_BRANCH_SANITIZED) reached the command — the latter regressed once,
    // producing `https://.api.<domain>` from an empty branch segment.
    let marker = f.path().join(".worktrees/feat-foo/api/env-marker");
    let got = std::fs::read_to_string(&marker).unwrap_or_default();
    assert_eq!(got, "feat-foo.web.myapp.test feat-foo", "host env missing WS_HOST_web or WS_BRANCH_SANITIZED");

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

#[test]
fn host_down_stops_the_real_server_behind_a_wrapper_shell() {
    // The recorded pid is only the dev server itself when every layer `exec`s
    // through. An `up` that points at a *script* leaves the wrapper shell alive as
    // a parent, so signalling the recorded pid alone killed the wrapper and left
    // the real server running, reparented to init and still holding its port —
    // one leaked process per `ws down`/`ws rm`.
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    // Marker file, then a long-lived child: the same shape as a dev server started
    // from a `.ws-hooks/` script.
    std::fs::write(
        f.path().join("runner.sh"),
        "#!/bin/sh\necho $$ > \"$WS_WORKSPACE_ROOT\"/wrapper.pid\nsleep 40 &\necho $! > \"$WS_WORKSPACE_ROOT\"/child.pid\nwait\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let p = f.path().join("runner.sh");
        let mut perms = std::fs::metadata(&p).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&p, perms).unwrap();
    }
    f.write_ws_json(
        r#"{"repos":[{"name":"api","kind":"host","services":{"web":{"port":8080}},
            "scripts":{"up":"\"$WS_WORKSPACE_ROOT\"/runner.sh"}}]}"#,
    );
    f.run_ok(&["new", "api", "feat/foo"]);
    f.run_ok(&["up", "feat/foo"]);

    let child_pid_file = f.path().join("child.pid");
    for _ in 0..40 {
        if child_pid_file.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let child = std::fs::read_to_string(&child_pid_file)
        .expect("the wrapper should have started a child")
        .trim()
        .to_string();
    let alive = |pid: &str| {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    assert!(alive(&child), "the child should be running before down");

    f.run_ok(&["down", "feat/foo"]);
    // Give the TERM a moment to land.
    for _ in 0..20 {
        if !alive(&child) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        !alive(&child),
        "the real server behind the wrapper must be stopped, not orphaned to init"
    );
    let _ = f.run(&["rm", "feat/foo", "--force"]);
}

#[test]
fn down_keeps_the_record_of_a_live_process_it_declines_to_kill() {
    // The identity guard is right to refuse to signal a stranger, but `down` used
    // to remove the pid file anyway — discarding the only record of a process it
    // had just left running. The next `ws up` then saw no pid file, concluded
    // nothing was running, and started a second dev server beside the first.
    let f = host_fixture();
    f.run_ok(&["new", "api", "feat/foo"]);
    f.run_ok(&["up", "feat/foo"]);

    let pid_file = f.path().join(".ws-run/ws-feat-foo-api.pid");
    let recorded = std::fs::read_to_string(&pid_file).expect("pid file after up");
    let pid = recorded.lines().next().unwrap().trim().to_string();

    // Same pid, wrong start-time identity — as a recycled pid would look.
    std::fs::write(&pid_file, format!("{pid}\nNotTheRealStartTime\n")).unwrap();

    let out = f.run(&["down", "feat/foo"]);
    let err = stderr(&out);

    let alive = |p: &str| {
        std::process::Command::new("kill")
            .args(["-0", p])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    assert!(alive(&pid), "an unmatched process must be left running, not signalled");
    assert!(
        pid_file.exists(),
        "the pid file must survive, or the process becomes untrackable and `up` starts a second one"
    );
    assert!(
        err.contains("identity doesn't match"),
        "leaving it running must be reported, not silent:\n{err}"
    );

    // Restore the real identity so teardown can actually stop it.
    std::fs::write(&pid_file, recorded).unwrap();
    stop(&f);
}
