//! `kind: host` repos are supervised as `systemd --user` transient units.
//! Uses fake `systemd-run`/`systemctl` (via WS_SYSTEMD_RUN / WS_SYSTEMCTL) that
//! record their argv, so the wiring is asserted deterministically without a
//! real systemd — the same seam the ssh/mutagen tests use.

mod common;

use common::{stderr, WorkspaceFixture};

fn host_fixture() -> WorkspaceFixture {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(
        r#"{"domain":"myapp.test","repos":[{"name":"api","kind":"host",
            "services":{"web":{"http":"web","port":8080}},
            "scripts":{"up":"cargo watch -x run"}}]}"#,
    );
    f
}

#[test]
fn host_up_launches_a_systemd_unit_with_the_injected_env() {
    let f = host_fixture();
    let sdr_args = f.path().join("systemd-run.args");
    let recorder = f.arg_recorder("fake-systemd-run");
    // is-active must report "not active" (exit 1) so `up` actually launches
    // rather than treating the unit as already running.
    let systemctl = f.stub("fake-systemctl", "exit 1");

    f.run_ok(&["new", "api", "feat/foo"]);
    let out = f.run_with_env(
        &["up", "feat/foo"],
        &[
            ("WS_SYSTEMD_RUN", recorder.to_str().unwrap()),
            ("WS_SYSTEMCTL", systemctl.to_str().unwrap()),
            ("ARGS_FILE", sdr_args.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "up failed:\n{}", stderr(&out));

    let args = std::fs::read_to_string(&sdr_args).expect("systemd-run should have been invoked");
    assert!(args.contains("--unit=ws-feat-foo-api"), "unit name missing:\n{args}");
    assert!(
        args.contains(".worktrees/feat-foo/api"),
        "working-directory should be the worktree:\n{args}"
    );
    assert!(args.contains("Restart=on-failure"), "crash-restart missing:\n{args}");
    // The injected routing env is forwarded as --setenv.
    assert!(
        args.contains("--setenv=WS_HOST_web=feat-foo.web.myapp.test"),
        "WS_HOST not forwarded:\n{args}"
    );
    assert!(args.lines().any(|l| l.starts_with("--setenv=WS_PORT_web=")),
        "a host HTTP service should also get a WS_PORT to bind:\n{args}");
    // The declared up command is what runs.
    assert!(args.contains("cargo watch -x run"), "up command missing:\n{args}");
}

#[test]
fn host_down_stops_the_unit() {
    let f = host_fixture();
    let sc_args = f.path().join("systemctl.args");
    let recorder = f.arg_recorder("fake-systemctl");

    f.run_ok(&["new", "api", "feat/foo"]);
    let out = f.run_with_env(
        &["down", "feat/foo"],
        &[
            ("WS_SYSTEMCTL", recorder.to_str().unwrap()),
            ("ARGS_FILE", sc_args.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "down failed:\n{}", stderr(&out));

    let args = std::fs::read_to_string(&sc_args).expect("systemctl should have been invoked");
    assert!(args.contains("stop"), "down should stop the unit:\n{args}");
    assert!(args.contains("ws-feat-foo-api"), "unit name missing:\n{args}");
}

#[test]
fn host_up_skips_when_the_unit_is_already_active() {
    let f = host_fixture();
    let sdr_args = f.path().join("systemd-run.args");
    let recorder = f.arg_recorder("fake-systemd-run");
    // is-active reports active (exit 0) → `up` must not re-launch.
    let systemctl = f.stub("fake-systemctl", "exit 0");

    f.run_ok(&["new", "api", "feat/foo"]);
    let out = f.run_with_env(
        &["up", "feat/foo"],
        &[
            ("WS_SYSTEMD_RUN", recorder.to_str().unwrap()),
            ("WS_SYSTEMCTL", systemctl.to_str().unwrap()),
            ("ARGS_FILE", sdr_args.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "up failed:\n{}", stderr(&out));
    assert!(
        !sdr_args.exists(),
        "systemd-run must not run when the unit is already active"
    );
}
