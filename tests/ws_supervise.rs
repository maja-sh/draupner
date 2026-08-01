//! `kind: host` repos are supervised as a detached, `ws`-owned process tracked by
//! a PID file (no systemd/launchd — works on macOS too). These run the *real*
//! mechanism: `ws up` launches a `sleep`, and we assert liveness via the pid file,
//! then `ws down` stops it. `$HOME` is redirected into the fixture, so `~/.ws-run`
//! is isolated.

mod common;

use common::{stderr, WorkspaceFixture};
use std::path::PathBuf;

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

/// Where `ws` records the pid of `feat/foo`'s api host process (under the
/// fixture HOME).
fn pid_file(f: &WorkspaceFixture) -> PathBuf {
    f.path().join(".ws-run/ws-feat-foo-api.pid")
}

/// Whether a pid is alive, probed the way the shell does.
fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Whether the host process for `feat/foo`'s api is alive, read the same way `ws`
/// does: the pid file under the fixture HOME, then `kill -0`.
fn proc_alive(f: &WorkspaceFixture) -> bool {
    let Ok(contents) = std::fs::read_to_string(pid_file(f)) else { return false };
    // The pid file is two lines — pid, then the start-time identity. We only need
    // the pid to probe liveness the way the shell does.
    let pid = contents.lines().next().unwrap_or("").trim();
    !pid.is_empty() && alive(pid)
}

/// Poll `cond` every 50 ms for up to 2 s, reporting whether it ever held.
///
/// Detached work — a supervised process starting, or dying after a signal —
/// races the assertion that follows it, and each wait below wants the same
/// budget rather than one invented per call site.
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

/// Runs its closure on drop — including while unwinding from a failed assertion.
///
/// A trailing `stop(&f)` only runs when every assertion above it passed, so a
/// failing test leaks the process it supervised: it is reparented to init and
/// keeps whatever it holds. The `sleep 40` fixtures self-limit, but the test that
/// serves a real `python3 -m http.server` does not — one such orphan was found
/// still LISTENing on 127.0.0.1:20000, the first port of ws's allocation range.
struct Teardown<F: FnMut()>(F);

impl<F: FnMut()> Drop for Teardown<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}

/// The pid `ws` recorded for `feat/foo`'s api.
fn recorded_pid(f: &WorkspaceFixture) -> String {
    let contents = std::fs::read_to_string(pid_file(f)).expect("pid file after up");
    contents.lines().next().unwrap().trim().to_string()
}

/// The host port ws allocated for `web`, read off its own output (`start web:20000`)
/// — the same handle a user has.
fn allocated_port(out: &str) -> u16 {
    let at = out.find("web:").expect("up should report the allocated port");
    out[at + 4..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .expect("a numeric port after `web:`")
}

/// Serialises the tests that care about *which* process holds an allocated port.
///
/// Allocation scans `.ws-meta` per workspace root and confirms the choice with a
/// live bind, so two fixtures running in parallel can be handed the same port
/// while neither is holding it yet — and then one test's listener gets attributed
/// to the other test's process. That collision is real (a known cross-workspace
/// limitation) but it isn't what these tests are about.
static PORT_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take the port lock, ignoring poisoning: one test failing shouldn't turn the
/// others into spurious failures that hide it.
fn port_lock() -> std::sync::MutexGuard<'static, ()> {
    PORT_TESTS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Can the site attribute a listening socket to a pid? `ss` (Linux) or `lsof`
/// (macOS); without either, ws reports plain liveness and these assertions
/// wouldn't apply.
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
fn list_reports_a_host_process_that_crashed() {
    // The state a user had no way to see: ws launched it, ws said green, and it
    // died some time later. `ws list` asked the repo's `status` script, which for a
    // host repo is usually absent — so a dead dev server looked exactly like a
    // healthy one (a bare dash) until something downstream failed.
    let f = host_fixture();
    f.run_ok(&["new", "api", "feat/foo"]);
    f.run_ok(&["up", "feat/foo"]);
    let pid = recorded_pid(&f);

    // Kill it the way a crash would — behind ws's back, no `ws down`.
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
    // Alive is not the same as serving. The fixture's `up` is a bare `sleep`: it
    // runs happily forever and never listens on the port ws handed it, which is
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
    // The expensive one. An orphan from an earlier run still held the port, so the
    // dev server found it busy and silently moved to another — while ws reported
    // the port it had allocated, and everything downstream dialled that.
    if !port_tools_available() {
        eprintln!("skipping: neither `ss` nor `lsof` available to attribute a port");
        return;
    }
    let f = host_fixture();
    f.run_ok(&["new", "api", "feat/foo"]);

    // First up allocates the port. The supervised process is a bare `sleep`, so it
    // never takes the port itself — exactly the situation the bug needs.
    let out = f.run_ok(&["up", "feat/foo"]);
    let port = allocated_port(&stderr(&out));

    // Stand in for the orphan: something outside ws's process tree holding it.
    // Tolerant of losing the bind — port allocation is scanned per workspace root
    // and probed live, so a fixture running in parallel can hold this port instead.
    // Either way something outside ws's tree has it, which is what's under test.
    let squatter = std::net::TcpListener::bind(("127.0.0.1", port)).ok();

    // A second up: the process is still running, and ws re-checks the port.
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
    // The green path: when the process really is listening on the port ws
    // allocated, ws says so plainly and adds no caveat.
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
    // Unlike the `sleep 40` fixtures, this one supervises a real HTTP server that
    // never exits on its own — teardown has to survive a failing assertion.
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

    // A second up must NOT relaunch — same pid, and it says "already running".
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
    wait_until(|| child_pid_file.exists());
    let child = std::fs::read_to_string(&child_pid_file)
        .expect("the wrapper should have started a child")
        .trim()
        .to_string();
    assert!(alive(&child), "the child should be running before down");

    f.run_ok(&["down", "feat/foo"]);
    // Give the TERM a moment to land.
    wait_until(|| !alive(&child));
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
    // Teardown written as the last statement of a test only runs when everything
    // above it passed — so the tests that supervise a process leaked it on exactly
    // the runs where a human most needs the machine left in a sane state. `Teardown`
    // is the guard that fixes that; this pins its behaviour on unwind.
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
    // Every wait above is a race against detached work, so both halves matter:
    // returning the instant the condition holds (or the tests pay the full budget
    // each time) and giving up after ~2 s (or a wait that never comes true hangs
    // the suite instead of failing its assertion).
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
