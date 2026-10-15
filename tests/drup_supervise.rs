mod common;

use common::{stderr, WorkspaceFixture};
use std::path::Path;

fn host_fixture() -> WorkspaceFixture {
    host_fixture_in(WorkspaceFixture::empty())
}

/// A [`host_fixture`] in a private port window above the default 20000..40000 so allocations can't collide with other tests.
fn host_fixture_at(port_base: u16) -> WorkspaceFixture {
    host_fixture_in(WorkspaceFixture::empty().with_port_base(port_base))
}

fn host_fixture_in(f: WorkspaceFixture) -> WorkspaceFixture {
    f.add_repo("api");
    f.write_ws_json(
        r#"{"domain":"myapp.test","repos":[{"name":"api","kind":"host",
            "services":{"web":{"http":"web","port":8080}},
            "scripts":{"up":"printf '%s %s' \"$DRUP_HOST_web\" \"$DRUP_BRANCH_SANITIZED\" > env-marker; exec sleep 40"}}]}"#,
    );
    f
}

fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn proc_alive(f: &WorkspaceFixture) -> bool {
    let pid_file = f.path().join(".drup-run/drup-feat-foo-api.pid");
    let Ok(contents) = std::fs::read_to_string(&pid_file) else { return false };
    // The pid file is two lines — pid then start-time identity; liveness only needs the pid.
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

/// Runs its closure on drop so a failed assertion doesn't leak the supervised process. A trailing `stop(&f)` only runs when everything above passed; without this, a real `python3 -m http.server` orphan was found still LISTENing on drup's first allocation port.
struct Teardown<F: FnMut()>(F);

impl<F: FnMut()> Drop for Teardown<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}

/// The pid `drup` recorded for `feat/foo`'s api.
fn recorded_pid(f: &WorkspaceFixture) -> String {
    let contents = std::fs::read_to_string(f.path().join(".drup-run/drup-feat-foo-api.pid"))
        .expect("pid file after up");
    contents.lines().next().unwrap().trim().to_string()
}

/// The host port drup allocated for `web`, parsed from its own output (`start web:20000`) — the same handle a user has.
fn allocated_port(out: &str) -> u16 {
    let at = out.find("web:").expect("up should report the allocated port");
    out[at + 4..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .expect("a numeric port after `web:`")
}

/// Non-overlapping port windows so parallel test processes can't be handed the same port and attribute each other's listeners.
const PORT_BASE_NEVER_BOUND: u16 = 41000;
const PORT_BASE_SQUATTED: u16 = 42000;
const PORT_BASE_SERVING: u16 = 43000;

/// Whether `ss` or `lsof` is present — without one, drup can't attribute a socket to a pid and these assertions don't apply.
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
    // DRUP_BRANCH_SANITIZED regressed once, producing `https://.api.<domain>` from an empty branch segment.
    let marker = f.path().join(".worktrees/feat-foo/api/env-marker");
    let got = std::fs::read_to_string(&marker).unwrap_or_default();
    assert_eq!(got, "feat-foo.web.myapp.test feat-foo", "host env missing DRUP_HOST_web or DRUP_BRANCH_SANITIZED");

    stop(&f);
    assert!(!proc_alive(&f), "host process should be gone after rm");
}

#[test]
fn list_reports_a_host_process_that_crashed() {
    // A dead host process used to look identical to a healthy one in `drup list`, since the `status` script is usually absent.
    let f = host_fixture();
    f.run_ok(&["new", "api", "feat/foo"]);
    f.run_ok(&["up", "feat/foo"]);
    let pid = recorded_pid(&f);

    // Kill it the way a crash would — behind drup's back, no `drup down`.
    std::process::Command::new("kill").args(["-9", &pid]).status().unwrap();
    for _ in 0..20 {
        if !proc_alive(&f) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    let out = f.run_ok(&["list"]);
    assert!(
        stderr(&out).contains("crashed"),
        "a host process that died must be visible in `drup list`:\n{}",
        stderr(&out)
    );
    stop(&f);
}

#[test]
fn list_reports_a_host_process_that_never_bound_its_allocated_port() {
    // Alive is not the same as serving: the fixture's bare `sleep` never binds its port yet passes a pid check.
    if !port_tools_available() {
        eprintln!("skipping: neither `ss` nor `lsof` available to attribute a port");
        return;
    }
    let f = host_fixture_at(PORT_BASE_NEVER_BOUND);
    f.run_ok(&["new", "api", "feat/foo"]);

    let out = f.run_ok(&["up", "feat/foo"]);
    let err = stderr(&out);
    assert!(
        err.contains("nothing is listening on the port drup allocated"),
        "up must say the port it reported isn't being served:\n{err}"
    );

    let out = f.run_ok(&["list"]);
    let port = allocated_port(&err);
    assert!(
        stderr(&out).contains(&format!("nothing on :{port}")),
        "`drup list` must name the unserved port:\n{}",
        stderr(&out)
    );
    stop(&f);
}

#[test]
fn up_warns_when_another_process_holds_the_allocated_port() {
    // An orphan holding the allocated port used to make the dev server silently switch ports while drup still reported the original.
    if !port_tools_available() {
        eprintln!("skipping: neither `ss` nor `lsof` available to attribute a port");
        return;
    }
    let f = host_fixture_at(PORT_BASE_SQUATTED);
    f.run_ok(&["new", "api", "feat/foo"]);

    // The supervised bare `sleep` never binds the port, which is the situation the bug needs.
    let out = f.run_ok(&["up", "feat/foo"]);
    let port = allocated_port(&stderr(&out));

    // Stand in for the orphan; tolerant of losing the bind since a parallel fixture may already hold this port — either way something outside drup's tree does.
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

    let f = WorkspaceFixture::empty().with_port_base(PORT_BASE_SERVING);
    f.add_repo("api");
    f.write_ws_json(
        r#"{"repos":[{"name":"api","kind":"host","services":{"web":{"port":8080}},
            "scripts":{"up":"exec python3 -m http.server \"$DRUP_PORT_web\" --bind 127.0.0.1"}}]}"#,
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
        "`drup list` must confirm the served port:\n{}",
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
    let pid1 = std::fs::read_to_string(f.path().join(".drup-run/drup-feat-foo-api.pid")).unwrap();

    // A second up must NOT relaunch — same pid, and it says "already running".
    let out = f.run_ok(&["up", "feat/foo"]);
    let pid2 = std::fs::read_to_string(f.path().join(".drup-run/drup-feat-foo-api.pid")).unwrap();
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
        !Path::new(&f.path().join(".drup-run/drup-feat-foo-api.pid")).exists(),
        "nothing to launch → no pid file"
    );
}

#[test]
fn host_down_stops_the_real_server_behind_a_wrapper_shell() {
    // Signalling only the recorded pid used to kill the wrapper shell and leak the real server, reparented to init and still holding its port.
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    // Wrapper shell backgrounding a child — the shape a dev server takes when started from a `.drup-hooks/` script.
    std::fs::write(
        f.path().join("runner.sh"),
        "#!/bin/sh\necho $$ > \"$DRUP_WORKSPACE_ROOT\"/wrapper.pid\nsleep 40 &\necho $! > \"$DRUP_WORKSPACE_ROOT\"/child.pid\nwait\n",
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
            "scripts":{"up":"\"$DRUP_WORKSPACE_ROOT\"/runner.sh"}}]}"#,
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
    // `down` used to remove the pid file even when the identity guard declined to signal, so the next `up` saw nothing and started a second server beside the first.
    let f = host_fixture();
    f.run_ok(&["new", "api", "feat/foo"]);
    f.run_ok(&["up", "feat/foo"]);

    let pid_file = f.path().join(".drup-run/drup-feat-foo-api.pid");
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

    for _ in 0..20 {
        if !alive(&pid) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        !alive(&pid),
        "a failed assertion must still tear the worktree down — pid {pid} was orphaned to init"
    );
}
