//! A `WSX_SSH` stub drives remote `up`/`down`/teardown deterministically; the real
//! podman+Caddy end-to-end lives in the self-SSH-to-localhost smoke, which needs
//! a live box and so isn't a unit test.

mod common;

use common::{stderr, WorkspaceFixture};
use std::path::{Path, PathBuf};

/// `WSX_SSH` stub whose branches feed each probe (`$HOME`, `ss`, `kill -0`, pid-file)
/// what `supervise::up` expects so a full remote `up`/`down` proceeds without a server.
fn ssh_stub(f: &WorkspaceFixture) -> PathBuf {
    f.stub(
        "fake-ssh",
        r#"printf '%s\n' "$@" >> "$ARGS_FILE"
last="${!#}"
case "$last" in
  *__WS_PATH_PROBE__*) printf '%s\n%s\n' __WS_PATH_PROBE__ /harvested/bin:/usr/bin ;;
  *'printf %s'*) printf /home/test ;;
  *'ss -Hltn'*) echo free ;;
  *'kill -0'*) exit 3 ;;
  *'timeout '*) echo running ;;
  *) exit 0 ;;
esac"#,
    )
}

/// The env pairs `run_with_env` needs to point ws at the ssh/mutagen stubs and record argv.
fn remote_env<'a>(ssh: &'a Path, mg: &'a Path, args: &'a Path) -> [(&'a str, &'a str); 3] {
    [
        ("WSX_SSH", ssh.to_str().unwrap()),
        ("WSX_MUTAGEN", mg.to_str().unwrap()),
        ("ARGS_FILE", args.to_str().unwrap()),
    ]
}

/// A no-op `WSX_MUTAGEN` stub (mutagen isn't installed on CI; sync is best-effort).
fn mutagen_noop(f: &WorkspaceFixture) -> PathBuf {
    f.stub("fake-mutagen", "exit 0")
}

fn remote_container_ws() -> &'static str {
    // No `domain` keeps routing off so `up` needs no Caddy, isolating dispatch + port allocation.
    r#"{
        "repos":[{"name":"api","services":{"mysql":{"port":3306}},
                  "scripts":{"up":"echo starting","down":"echo stopping"}}],
        "remote":{"connection":"srv","workspace_path":"ws"}
    }"#
}

/// Creates the worktree so a following `up`/`down` reuses its HOME resolution and recorded port allocation.
fn new_remote(f: &WorkspaceFixture, repo: &str, ssh: &Path, mg: &Path) {
    let out = f.run_with_env(
        &["new", repo, "feat/foo"],
        &[
            ("WSX_SSH", ssh.to_str().unwrap()),
            ("WSX_MUTAGEN", mg.to_str().unwrap()),
            ("ARGS_FILE", f.path().join("new-ssh.txt").to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "wsx new failed:\n{}", stderr(&out));
}

#[test]
fn up_runs_the_script_on_the_server_over_ssh() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(remote_container_ws());
    let ssh = ssh_stub(&f);
    let mg = mutagen_noop(&f);
    new_remote(&f, "api", &ssh, &mg);

    let args = f.path().join("up-ssh.txt");
    let out = f.run_with_env(
        &["up", "feat/foo"],
        &[
            ("WSX_SSH", ssh.to_str().unwrap()),
            ("WSX_MUTAGEN", mg.to_str().unwrap()),
            ("ARGS_FILE", args.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "wsx up failed:\n{}", stderr(&out));

    let rec = std::fs::read_to_string(&args).expect("ssh was invoked");
    let line = rec
        .lines()
        .find(|l| l.contains("exec sh -c 'echo starting'"))
        .unwrap_or_else(|| panic!("up script not dispatched over ssh:\n{rec}"));
    // A relative base leaves WSX_WORKSPACE_ROOT relative and breaks `$WSX_WORKSPACE_ROOT/…` in a hook.
    assert!(line.contains("cd /home/test/ws/feat-foo/api &&"), "wrong server dir:\n{line}");
    assert!(line.contains("WSX_WORKSPACE_ROOT=/home/test/ws"), "WSX_WORKSPACE_ROOT not absolute:\n{line}");
    assert!(line.contains("WSX_REPO=api"), "missing WSX_REPO:\n{line}");
    assert!(line.contains("WSX_PORT_mysql="), "missing allocated port env:\n{line}");
    // A shared hook branches on WSX_REMOTE/WSX_RUNTIME (synced dump vs same-host copy; docker vs podman).
    assert!(line.contains("WSX_REMOTE=1"), "missing WSX_REMOTE=1 on the server:\n{line}");
    assert!(line.contains("WSX_RUNTIME="), "missing WSX_RUNTIME:\n{line}");
}

/// A workspace whose service name carries a dash — `redis-cache` is ordinary
/// compose naming, and it makes `WSX_PORT_redis-cache`, which is *not* a valid
/// shell name. Declared here rather than derived so the test doesn't need a
/// compose binary; the env key is byte-identical either way.
fn dashed_service_ws() -> &'static str {
    r#"{
        "repos":[{"name":"api","services":{"redis-cache":{"port":6379}},
                  "scripts":{"up":"echo starting","status":"docker compose ps"}}],
        "remote":{"connection":"srv","workspace_path":"ws"}
    }"#
}

#[test]
fn a_dashed_service_name_keeps_the_up_script_runnable() {
    // `WSX_PORT_redis-cache=20001` as a bare assignment word isn't an assignment to
    // the shell — it's the command. The whole remote script used to die with
    // `WSX_PORT_redis-cache=20001: not found` (exit 127, reported as `up script
    // exited 127`) and the hook never ran, on a workspace that works locally.
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(dashed_service_ws());
    let ssh = ssh_stub(&f);
    let mg = mutagen_noop(&f);
    new_remote(&f, "api", &ssh, &mg);

    let args = f.path().join("up-ssh.txt");
    let out = f.run_with_env(&["up", "feat/foo"], &remote_env(&ssh, &mg, &args));
    assert!(out.status.success(), "wsx up failed:\n{}", stderr(&out));

    let rec = std::fs::read_to_string(&args).expect("ssh was invoked");
    let line = rec
        .lines()
        .find(|l| l.contains("sh -c 'echo starting'"))
        .unwrap_or_else(|| panic!("up script not dispatched over ssh:\n{rec}"));
    // The pairs go through `env`, which accepts any name — as the local path's
    // `Command::env` does.
    assert!(line.contains("exec env "), "env pairs not handed to `env`:\n{line}");
    assert!(line.contains("WSX_PORT_redis-cache="), "dashed port var not injected:\n{line}");
}

#[test]
fn a_dashed_service_name_keeps_the_status_probe_runnable() {
    // Same key, the `wsx list` path. Here the breakage was silent: the exit 127 made
    // `ssh_capture` return `None`, so the status column showed the missing-value
    // dash with no error anywhere.
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(dashed_service_ws());
    let ssh = ssh_stub(&f);
    let mg = mutagen_noop(&f);
    new_remote(&f, "api", &ssh, &mg);

    let args = f.path().join("list-ssh.txt");
    let out = f.run_with_env(&["list"], &remote_env(&ssh, &mg, &args));
    assert!(out.status.success(), "wsx list failed:\n{}", stderr(&out));

    let rec = std::fs::read_to_string(&args).expect("ssh was invoked");
    let line = rec
        .lines()
        .find(|l| l.contains("timeout 5 sh -c 'docker compose ps'"))
        .unwrap_or_else(|| panic!("status not run on the server:\n{rec}"));
    assert!(line.contains("&& env "), "env pairs not handed to `env`:\n{line}");
    assert!(line.contains("WSX_PORT_redis-cache="), "dashed port var not injected:\n{line}");
    // …and the server's answer still reaches the listing.
    let shown = format!("{}{}", common::stdout(&out), stderr(&out));
    assert!(shown.contains("running"), "status not surfaced in the listing:\n{shown}");
}

#[test]
fn down_runs_the_script_on_the_server_over_ssh() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(remote_container_ws());
    let ssh = ssh_stub(&f);
    let mg = mutagen_noop(&f);
    new_remote(&f, "api", &ssh, &mg);

    let args = f.path().join("down-ssh.txt");
    let out = f.run_with_env(
        &["down", "feat/foo"],
        &[
            ("WSX_SSH", ssh.to_str().unwrap()),
            ("WSX_MUTAGEN", mg.to_str().unwrap()),
            ("ARGS_FILE", args.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "wsx down failed:\n{}", stderr(&out));

    let rec = std::fs::read_to_string(&args).expect("ssh was invoked");
    assert!(
        rec.lines().any(|l| l.contains("cd /home/test/ws/feat-foo/api &&")
            && l.contains("exec sh -c 'echo stopping'")),
        "down script not dispatched over ssh:\n{rec}"
    );
}

#[test]
fn down_degrades_to_local_cleanup_when_the_server_is_unreachable() {
    // Parity with `wsx rm`: a `down` on an unreachable server used to abort at
    // `Site::resolve`, orphaning local mutagen forwards even though the
    // teardown code that would free them is entirely local. Now `down` runs
    // its local cleanup and warns about the server-side leftovers instead.
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(remote_container_ws());
    let ssh = ssh_stub(&f);
    let mg = mutagen_noop(&f);
    new_remote(&f, "api", &ssh, &mg);

    // Swap in a failing ssh so any server round-trip errors — the reachability
    // probe inside `Site::resolve` (`printf $HOME`) is what trips.
    let bad = f.stub("bad-ssh", "exit 1");
    let out = f.run_with_env(
        &["down", "feat/foo"],
        &[
            ("WSX_SSH", bad.to_str().unwrap()),
            ("WSX_MUTAGEN", mg.to_str().unwrap()),
        ],
    );

    assert!(out.status.success(), "down must not bail when the server is unreachable:\n{}", stderr(&out));
    let err = stderr(&out);
    assert!(
        err.contains("server unreachable") && err.contains("local forwards"),
        "expected the degrade note in stderr:\n{err}"
    );
}

#[test]
fn list_runs_status_on_the_server_for_a_remote_workspace() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(
        r#"{
            "repos":[{"name":"api","services":{"mysql":{"port":3306}},
                      "scripts":{"up":"echo x","status":"docker compose ps"}}],
            "remote":{"connection":"srv","workspace_path":"ws"}
        }"#,
    );
    let ssh = ssh_stub(&f);
    let mg = mutagen_noop(&f);
    new_remote(&f, "api", &ssh, &mg);

    let args = f.path().join("list-ssh.txt");
    let out = f.run_with_env(
        &["list"],
        &[
            ("WSX_SSH", ssh.to_str().unwrap()),
            ("WSX_MUTAGEN", mg.to_str().unwrap()),
            ("ARGS_FILE", args.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "wsx list failed:\n{}", stderr(&out));

    let rec = std::fs::read_to_string(&args).expect("ssh was invoked");
    assert!(
        rec.lines().any(|l| l.contains("cd /home/test/ws/feat-foo/api &&")
            && l.contains("timeout 5 sh -c 'docker compose ps'")),
        "status not run on the server:\n{rec}"
    );
    let shown = format!("{}{}", common::stdout(&out), stderr(&out));
    assert!(shown.contains("running"), "status not surfaced in the listing:\n{shown}");
}

#[test]
fn host_repo_on_a_remote_workspace_runs_on_the_server() {
    let f = WorkspaceFixture::empty();
    f.add_repo("svc");
    f.write_ws_json(
        r#"{
            "repos":[{"name":"svc","kind":"host","services":{"web":{"port":8080}},
                      "scripts":{"up":"run --port $WSX_PORT_web"}}],
            "remote":{"connection":"srv","workspace_path":"ws"}
        }"#,
    );
    let ssh = ssh_stub(&f);
    let mg = mutagen_noop(&f);
    new_remote(&f, "svc", &ssh, &mg);

    let args = f.path().join("up-ssh.txt");
    let out = f.run_with_env(
        &["up", "feat/foo"],
        &[
            ("WSX_SSH", ssh.to_str().unwrap()),
            ("WSX_MUTAGEN", mg.to_str().unwrap()),
            ("ARGS_FILE", args.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "wsx up failed:\n{}", stderr(&out));

    let rec = std::fs::read_to_string(&args).expect("ssh was invoked");
    assert!(rec.contains("nohup "), "host process not launched (no nohup) over ssh:\n{rec}");
    assert!(
        rec.contains("cd /home/test/ws/feat-foo/svc"),
        "launched from the server worktree:\n{rec}"
    );
    assert!(rec.contains("wsx-feat-foo-svc.pid"), "pid not recorded under ~/.wsx-run:\n{rec}");
    assert!(rec.contains("WSX_PORT_web="), "allocated port not injected:\n{rec}");
    assert!(rec.contains("sh -c 'run --port $WSX_PORT_web'"), "up cmd not via sh -c:\n{rec}");
    // Without the harvested login PATH on the supervisor path, a `#!/usr/bin/env node` shebang dies in the log after the step reported green.
    assert!(
        rec.contains("PATH=/harvested/bin:/usr/bin"),
        "harvested login PATH not passed to the detached host process:\n{rec}"
    );
    // WSX_WORKSPACE_ROOT anchors `.wsx-hooks/…` paths, so it must name the server's root, not the client's.
    assert!(
        rec.contains("WSX_WORKSPACE_ROOT=/home/test/ws"),
        "WSX_WORKSPACE_ROOT must be the server-side root for a detached host process:\n{rec}"
    );
}

#[test]
fn a_host_process_that_dies_on_launch_fails_the_step_and_shows_its_log() {
    // `nohup … &` always yields a pid, so a process that dies on launch used to report green with the reason buried in a log; the launch script now re-checks and returns the tail.
    let f = WorkspaceFixture::empty();
    f.add_repo("svc");
    f.write_ws_json(
        r#"{
            "repos":[{"name":"svc","kind":"host","services":{"web":{"port":8080}},
                      "scripts":{"up":"run --port $WSX_PORT_web"}}],
            "remote":{"connection":"srv","workspace_path":"ws"}
        }"#,
    );
    // Mirrors the real launch script's output when the process is already gone.
    let ssh = f.stub(
        "fake-ssh-dead",
        r#"printf '%s\n' "$@" >> "$ARGS_FILE"
last="${!#}"
case "$last" in
  *__WS_PATH_PROBE__*) printf '%s\n%s\n' __WS_PATH_PROBE__ /harvested/bin:/usr/bin ;;
  *nohup*) echo __WS_HOST_DIED__; echo "/usr/bin/env: 'node': No such file or directory" ;;
  *'printf %s'*) printf /home/test ;;
  *'ss -Hltn'*) echo free ;;
  *'kill -0'*) exit 3 ;;
  *) exit 0 ;;
esac"#,
    );
    let mg = mutagen_noop(&f);
    new_remote(&f, "svc", &ssh, &mg);

    let args = f.path().join("up-ssh.txt");
    let out = f.run_with_env(
        &["up", "feat/foo"],
        &[
            ("WSX_SSH", ssh.to_str().unwrap()),
            ("WSX_MUTAGEN", mg.to_str().unwrap()),
            ("ARGS_FILE", args.to_str().unwrap()),
        ],
    );

    let err = stderr(&out);
    assert!(!out.status.success(), "a dead host process must fail `wsx up`:\n{err}");
    assert!(
        err.contains("exited immediately"),
        "the step must say the process died, not report it started:\n{err}"
    );
    // The reason is the entire content of this failure — it belongs on screen.
    assert!(
        err.contains("No such file or directory"),
        "the log tail must be surfaced, not left in ~/.wsx-run:\n{err}"
    );
}

#[test]
fn prepare_runs_on_the_client_while_up_dispatches_to_the_server() {
    // `prepare` stays on the client even when offloading so it can stage local data into the worktree for the sync to carry.
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(remote_container_ws());
    f.add_hook("api", "prepare", "echo staged > \"$WSX_WORKTREE/prepared.txt\"");
    let ssh = ssh_stub(&f);
    let mg = mutagen_noop(&f);
    new_remote(&f, "api", &ssh, &mg);

    let args = f.path().join("up-ssh.txt");
    let out = f.run_with_env(
        &["up", "feat/foo"],
        &[
            ("WSX_SSH", ssh.to_str().unwrap()),
            ("WSX_MUTAGEN", mg.to_str().unwrap()),
            ("ARGS_FILE", args.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "wsx up failed:\n{}", stderr(&out));

    assert!(
        f.path().join(".worktrees/feat-foo/api/prepared.txt").exists(),
        "prepare must run on the client for a remote workspace"
    );
    let rec = std::fs::read_to_string(&args).expect("ssh was invoked");
    assert!(rec.contains("exec sh -c 'echo starting'"), "up must dispatch over ssh:\n{rec}");
}

#[test]
fn ws_hooks_are_pushed_to_the_server_before_a_remote_hook() {
    // A workspace-root `.wsx-hooks/` lives outside the repos and can't ride the worktree sync, so `wsx` pushes it so a hook can reference `$WSX_WORKSPACE_ROOT/.wsx-hooks/…` there.
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(remote_container_ws());
    std::fs::create_dir_all(f.path().join(".wsx-hooks")).unwrap();
    std::fs::write(f.path().join(".wsx-hooks/up.sh"), "#!/usr/bin/env bash\necho hi\n").unwrap();
    let ssh = ssh_stub(&f);
    let mg = mutagen_noop(&f);
    new_remote(&f, "api", &ssh, &mg);

    let args = f.path().join("up-ssh.txt");
    let out = f.run_with_env(
        &["up", "feat/foo"],
        &[
            ("WSX_SSH", ssh.to_str().unwrap()),
            ("WSX_MUTAGEN", mg.to_str().unwrap()),
            ("ARGS_FILE", args.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "wsx up failed:\n{}", stderr(&out));

    let rec = std::fs::read_to_string(&args).expect("ssh was invoked");
    assert!(
        rec.lines().any(|l| l.contains("tar -C /home/test/ws") && l.contains("-xpf -")),
        "expected a .wsx-hooks push (tar extract) over ssh:\n{rec}"
    );
    // `-xpf` preserves mode, so a hook rewritten as 644 would land unrunnable and fail at hook time with a bare `Permission denied` (exit 126).
    assert!(
        rec.lines().any(|l| l.contains("chmod +x") && l.contains("#!")),
        "expected the push to restore +x on shebang scripts:\n{rec}"
    );
}
