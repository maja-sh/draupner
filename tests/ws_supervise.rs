//! These drive the real supervision mechanism — `ws up` launches an actual
//! process. The fixture redirects `$HOME`, so the pid files land under the
//! fixture's own `~/.ws-run` rather than the developer's.

mod common;

use common::{stderr, WorkspaceFixture};
use std::path::PathBuf;

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

fn pid_file(f: &WorkspaceFixture) -> PathBuf {
    f.path().join(".ws-run/ws-feat-foo-api.pid")
}

fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn proc_alive(f: &WorkspaceFixture) -> bool {
    let Ok(contents) = std::fs::read_to_string(pid_file(f)) else { return false };
    // The pid file is two lines: the pid, then the start-time identity.
    let pid = contents.lines().next().unwrap_or("").trim();
    !pid.is_empty() && alive(pid)
}

/// Every assertion about a supervised process races the detached work that
/// satisfies it, so they all share one polling budget rather than inventing one
/// per call site.
fn wait_until(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..40 {
        if cond() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

fn stop(f: &WorkspaceFixture) {
    // Always tear the worktree down so no detached `sleep` leaks past the test.
    let _ = f.run(&["rm", "feat/foo", "--force"]);
}

/// A trailing `stop(&f)` only runs when every assertion above it passed, so a
/// failing test orphans the process it supervised to init, still holding its
/// port. This tears down on unwind too.
struct Teardown<F: FnMut()>(F);

impl<F: FnMut()> Drop for Teardown<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}

fn recorded_pid(f: &WorkspaceFixture) -> String {
    let contents = std::fs::read_to_string(pid_file(f)).expect("pid file after up");
    contents.lines().next().unwrap().trim().to_string()
}

fn allocated_port(out: &str) -> u16 {
    let at = out.find("web:").expect("up should report the allocated port");
    out[at + 4..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .expect("a numeric port after `web:`")
}

/// Port allocation is scanned per workspace root, so two fixtures in parallel can
/// be handed the same free port and then attribute each other's listener. Tests
/// that care *which* process holds a port take this lock.
static PORT_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Poisoning is ignored so one failing test doesn't turn the rest into spurious
/// failures that hide it.
fn port_lock() -> std::sync::MutexGuard<'static, ()> {
    PORT_TESTS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Without `ss` or `lsof` ws reports plain liveness, so the port assertions below
/// have nothing to assert.
fn port_tools_available() -> bool {
    ["ss", "lsof"].iter().any(|bin| {
        std::process::Command::new("sh")
            .args(["-c", &format!("command -v {bin}")])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

#[test]
fn host_up_launches_a_detached_process_with_injected_env() {
    let f = host_fixture();
    f.run_ok(&["new", "api", "feat/foo"]);

    let out = f.run_ok(&["up", "feat/foo"]);
    assert!(out.status.success(), "up failed:\n{}", stderr(&out));

    assert!(proc_alive(&f), "host process should be running after up");
    // WS_BRANCH_SANITIZED is asserted alongside the routing env because it
    // regressed once, producing `https://.api.<domain>` from an empty segment.
    let marker = f.path().join(".worktrees/feat-foo/api/env-marker");
    let got = std::fs::read_to_string(&marker).unwrap_or_default();
    assert_eq!(got, "feat-foo.web.myapp.test feat-foo", "host env missing WS_HOST_web or WS_BRANCH_SANITIZED");

    stop(&f);
    assert!(!proc_alive(&f), "host process should be gone after rm");
}

#[test]
fn list_reports_a_host_process_that_crashed() {
    let f = host_fixture();
    f.run_ok(&["new", "api", "feat/foo"]);
    f.run_ok(&["up", "feat/foo"]);
    let pid = recorded_pid(&f);

    std::process::Command::new("kill").args(["-9", &pid]).status().unwrap();
    wait_until(|| !proc_alive(&f));

    let out = f.run_ok(&["list"]);
    assert!(
        stderr(&out).contains("crashed"),
        "a host process that died must be visible in `ws list`:\n{}",
        stderr(&out)
    );
    stop(&f);
}

#[test]
fn list_reports_a_host_process_that_never_bound_its_allocated_port() {
    let _ports = port_lock();
    // The fixture's `up` is a bare `sleep` — alive forever, never listening, and
    // indistinguishable from a working dev server to a pid check.
    if !port_tools_available() {
        eprintln!("skipping: neither `ss` nor `lsof` available to attribute a port");
        return;
    }
    let f = host_fixture();
    f.run_ok(&["new", "api", "feat/foo"]);

    let out = f.run_ok(&["up", "feat/foo"]);
    let err = stderr(&out);
    assert!(
        err.contains("nothing is listening on the port ws allocated"),
        "up must say the port it reported isn't being served:\n{err}"
    );

    let out = f.run_ok(&["list"]);
    let port = allocated_port(&err);
    assert!(
        stderr(&out).contains(&format!("nothing on :{port}")),
        "`ws list` must name the unserved port:\n{}",
        stderr(&out)
    );
    stop(&f);
}

#[test]
fn up_warns_when_another_process_holds_the_allocated_port() {
    let _ports = port_lock();
    if !port_tools_available() {
        eprintln!("skipping: neither `ss` nor `lsof` available to attribute a port");
        return;
    }
    let f = host_fixture();
    f.run_ok(&["new", "api", "feat/foo"]);

    // The supervised process is a bare `sleep`, so it never takes the allocated
    // port itself — leaving it free for the squatter below.
    let out = f.run_ok(&["up", "feat/foo"]);
    let port = allocated_port(&stderr(&out));

    // Losing the bind is fine: a parallel fixture holding the port is still
    // something outside ws's process tree, which is what's under test.
    let squatter = std::net::TcpListener::bind(("127.0.0.1", port)).ok();

    let out = f.run_ok(&["up", "feat/foo"]);
    let err = stderr(&out);
    drop(squatter);
    stop(&f);

    assert!(
        err.contains("another process already holds the allocated port"),
        "up must flag a port held outside its own process tree:\n{err}"
    );
    assert!(
        err.contains(&format!(":{port} is held by something outside this process")),
        "the warning must name the port:\n{err}"
    );
}

#[test]
fn up_and_list_confirm_a_process_serving_its_allocated_port() {
    let _ports = port_lock();
    if !port_tools_available() {
        eprintln!("skipping: neither `ss` nor `lsof` available to attribute a port");
        return;
    }
    let python = std::process::Command::new("sh")
        .args(["-c", "command -v python3"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !python {
        eprintln!("skipping: no `python3` to run a listener");
        return;
    }

    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(
        r#"{"repos":[{"name":"api","kind":"host","services":{"web":{"port":8080}},
            "scripts":{"up":"exec python3 -m http.server \"$WS_PORT_web\" --bind 127.0.0.1"}}]}"#,
    );
    f.run_ok(&["new", "api", "feat/foo"]);
    // Unlike the `sleep 40` fixtures, this server never exits on its own, so
    // teardown has to survive a failing assertion.
    let _teardown = Teardown(|| stop(&f));

    let out = f.run_ok(&["up", "feat/foo"]);
    let err = stderr(&out);
    let port = allocated_port(&err);
    assert!(
        !err.contains("nothing is listening") && !err.contains("another process already holds"),
        "a server that bound its port must draw no complaint:\n{err}"
    );

    let out = f.run_ok(&["list"]);
    assert!(
        stderr(&out).contains(&format!("running :{port}")),
        "`ws list` must confirm the served port:\n{}",
        stderr(&out)
    );
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
    let pid1 = std::fs::read_to_string(pid_file(&f)).unwrap();

    let out = f.run_ok(&["up", "feat/foo"]);
    let pid2 = std::fs::read_to_string(pid_file(&f)).unwrap();
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
    assert!(!pid_file(&f).exists(), "nothing to launch → no pid file");
}

#[test]
fn host_down_stops_the_real_server_behind_a_wrapper_shell() {
    // The recorded pid is the dev server itself only when every layer `exec`s
    // through. This fixture is the shape that doesn't — a wrapper script with a
    // long-lived child — where signalling the recorded pid alone kills the wrapper
    // and orphans the real server.
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
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
    wait_until(|| child_pid_file.exists());
    let child = std::fs::read_to_string(&child_pid_file)
        .expect("the wrapper should have started a child")
        .trim()
        .to_string();
    assert!(alive(&child), "the child should be running before down");

    f.run_ok(&["down", "feat/foo"]);
    wait_until(|| !alive(&child));
    assert!(
        !alive(&child),
        "the real server behind the wrapper must be stopped, not orphaned to init"
    );
    let _ = f.run(&["rm", "feat/foo", "--force"]);
}

#[test]
fn down_keeps_the_record_of_a_live_process_it_declines_to_kill() {
    let f = host_fixture();
    f.run_ok(&["new", "api", "feat/foo"]);
    f.run_ok(&["up", "feat/foo"]);

    let pid_file = pid_file(&f);
    let recorded = std::fs::read_to_string(&pid_file).expect("pid file after up");
    let pid = recorded.lines().next().unwrap().trim().to_string();

    // Same pid, wrong start-time identity — as a recycled pid would look.
    std::fs::write(&pid_file, format!("{pid}\nNotTheRealStartTime\n")).unwrap();

    let out = f.run(&["down", "feat/foo"]);
    let err = stderr(&out);

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

#[test]
fn teardown_runs_when_an_assertion_fails_mid_test() {
    let f = host_fixture();
    f.run_ok(&["new", "api", "feat/foo"]);
    f.run_ok(&["up", "feat/foo"]);
    let pid = recorded_pid(&f);
    assert!(alive(&pid), "precondition: the supervised process should be running");

    let result: Result<(), _> = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _teardown = Teardown(|| stop(&f));
        panic!("stand-in for a failed assertion");
    }));
    assert!(result.is_err(), "precondition: the guarded block must have panicked");

    wait_until(|| !alive(&pid));
    assert!(
        !alive(&pid),
        "a failed assertion must still tear the worktree down — pid {pid} was orphaned to init"
    );
}

#[test]
fn wait_until_returns_early_and_gives_up_after_its_budget() {
    let mut polls = 0;
    let started = std::time::Instant::now();
    assert!(wait_until(|| {
        polls += 1;
        polls == 3
    }));
    assert_eq!(polls, 3, "must stop polling as soon as the condition holds");
    assert!(
        started.elapsed() < std::time::Duration::from_millis(1000),
        "an early return must not sleep out the budget, took {:?}",
        started.elapsed()
    );

    let started = std::time::Instant::now();
    assert!(!wait_until(|| false), "a condition that never holds must report failure");
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(1500),
        "the budget is ~2 s; waiting only {:?} would race the detached work it exists for",
        started.elapsed()
    );
}
