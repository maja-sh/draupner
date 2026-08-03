//! These assert the SSH dispatch against a `WS_SSH` stub. The real end-to-end
//! (podman + Caddy on a server) is the self-SSH-to-localhost smoke, which needs a
//! live box and so isn't a unit test.

mod common;

use common::{stderr, WorkspaceFixture};
use std::path::{Path, PathBuf};

/// Each arm answers one probe in the shape its caller parses: `free` on stdout is
/// what the `ss` port probe reads, and a non-zero `kill -0` reports the host
/// process as not-running so `supervise::up` proceeds to launch it.
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

/// Mutagen isn't installed on CI, and sync is best-effort, so a no-op stands in.
fn remote_fixture(repo: &str, ws_json: &str) -> (WorkspaceFixture, PathBuf, PathBuf) {
    let f = WorkspaceFixture::empty();
    f.add_repo(repo);
    f.write_ws_json(ws_json);
    let ssh = ssh_stub(&f);
    let mg = f.stub("fake-mutagen", "exit 0");
    (f, ssh, mg)
}

fn remote_env<'a>(ssh: &'a Path, mg: &'a Path, args: &'a Path) -> [(&'a str, &'a str); 3] {
    [
        ("WS_SSH", ssh.to_str().unwrap()),
        ("WS_MUTAGEN", mg.to_str().unwrap()),
        ("ARGS_FILE", args.to_str().unwrap()),
    ]
}

fn remote_container_ws() -> &'static str {
    // No `domain` ⇒ routing off, so `up` needs no Caddy — this isolates the
    // script-dispatch + port-allocation path.
    r#"{
        "repos":[{"name":"api","services":{"mysql":{"port":3306}},
                  "scripts":{"up":"echo starting","down":"echo stopping"}}],
        "remote":{"connection":"srv","workspace_path":"ws"}
    }"#
}

fn host_repo_ws() -> &'static str {
    r#"{
        "repos":[{"name":"svc","kind":"host","services":{"web":{"port":8080}},
                  "scripts":{"up":"run --port $WS_PORT_web"}}],
        "remote":{"connection":"srv","workspace_path":"ws"}
    }"#
}

fn new_remote(f: &WorkspaceFixture, repo: &str, ssh: &Path, mg: &Path) {
    let args = f.path().join("new-ssh.txt");
    f.run_env_ok(&["new", repo, "feat/foo"], &remote_env(ssh, mg, &args));
}

#[test]
fn up_runs_the_script_on_the_server_over_ssh() {
    let (f, ssh, mg) = remote_fixture("api", remote_container_ws());
    new_remote(&f, "api", &ssh, &mg);

    let args = f.path().join("up-ssh.txt");
    f.run_env_ok(&["up", "feat/foo"], &remote_env(&ssh, &mg, &args));

    let rec = std::fs::read_to_string(&args).expect("ssh was invoked");
    let line = rec
        .lines()
        .find(|l| l.contains("exec sh -c 'echo starting'"))
        .unwrap_or_else(|| panic!("up script not dispatched over ssh:\n{rec}"));
    // The server dir must be home-resolved: a relative base leaves
    // WS_WORKSPACE_ROOT relative and breaks `$WS_WORKSPACE_ROOT/…` in a hook.
    assert!(line.contains("cd /home/test/ws/feat-foo/api &&"), "wrong server dir:\n{line}");
    assert!(line.contains("WS_WORKSPACE_ROOT=/home/test/ws"), "WS_WORKSPACE_ROOT not absolute:\n{line}");
    assert!(line.contains("WS_REPO=api"), "missing WS_REPO:\n{line}");
    assert!(line.contains("WS_PORT_mysql="), "missing allocated port env:\n{line}");
    // A hook shared between local and remote branches on these two.
    assert!(line.contains("WS_REMOTE=1"), "missing WS_REMOTE=1 on the server:\n{line}");
    assert!(line.contains("WS_RUNTIME="), "missing WS_RUNTIME:\n{line}");
}

/// `redis-cache` is ordinary compose naming, and it makes `WS_PORT_redis-cache`,
/// which is *not* a valid shell name. Declared rather than derived so the test
/// needs no compose binary; the env key is byte-identical either way.
fn dashed_service_ws() -> &'static str {
    r#"{
        "repos":[{"name":"api","services":{"redis-cache":{"port":6379}},
                  "scripts":{"up":"echo starting","status":"docker compose ps"}}],
        "remote":{"connection":"srv","workspace_path":"ws"}
    }"#
}

#[test]
fn a_dashed_service_name_keeps_the_up_script_runnable() {
    // A bare `WS_PORT_redis-cache=20001` word isn't an assignment to the shell —
    // it's the command, and the whole remote script dies with exit 127.
    let (f, ssh, mg) = remote_fixture("api", dashed_service_ws());
    new_remote(&f, "api", &ssh, &mg);

    let args = f.path().join("up-ssh.txt");
    f.run_env_ok(&["up", "feat/foo"], &remote_env(&ssh, &mg, &args));

    let rec = std::fs::read_to_string(&args).expect("ssh was invoked");
    let line = rec
        .lines()
        .find(|l| l.contains("sh -c 'echo starting'"))
        .unwrap_or_else(|| panic!("up script not dispatched over ssh:\n{rec}"));
    // `env` accepts any name, as the local path's `Command::env` does.
    assert!(line.contains("exec env "), "env pairs not handed to `env`:\n{line}");
    assert!(line.contains("WS_PORT_redis-cache="), "dashed port var not injected:\n{line}");
}

#[test]
fn a_dashed_service_name_keeps_the_status_probe_runnable() {
    // Same key as above, but here the exit 127 was silent — `ssh_capture` returned
    // `None` and the status column showed its missing-value dash. Hence the final
    // assertion that the server's answer actually reaches the listing.
    let (f, ssh, mg) = remote_fixture("api", dashed_service_ws());
    new_remote(&f, "api", &ssh, &mg);

    let args = f.path().join("list-ssh.txt");
    let out = f.run_env_ok(&["list"], &remote_env(&ssh, &mg, &args));

    let rec = std::fs::read_to_string(&args).expect("ssh was invoked");
    let line = rec
        .lines()
        .find(|l| l.contains("timeout 5 sh -c 'docker compose ps'"))
        .unwrap_or_else(|| panic!("status not run on the server:\n{rec}"));
    assert!(line.contains("&& env "), "env pairs not handed to `env`:\n{line}");
    assert!(line.contains("WS_PORT_redis-cache="), "dashed port var not injected:\n{line}");
    let shown = format!("{}{}", common::stdout(&out), stderr(&out));
    assert!(shown.contains("running"), "status not surfaced in the listing:\n{shown}");
}

#[test]
fn down_runs_the_script_on_the_server_over_ssh() {
    let (f, ssh, mg) = remote_fixture("api", remote_container_ws());
    new_remote(&f, "api", &ssh, &mg);

    let args = f.path().join("down-ssh.txt");
    f.run_env_ok(&["down", "feat/foo"], &remote_env(&ssh, &mg, &args));

    let rec = std::fs::read_to_string(&args).expect("ssh was invoked");
    assert!(
        rec.lines().any(|l| l.contains("cd /home/test/ws/feat-foo/api &&")
            && l.contains("exec sh -c 'echo stopping'")),
        "down script not dispatched over ssh:\n{rec}"
    );
}

#[test]
fn list_runs_status_on_the_server_for_a_remote_workspace() {
    let (f, ssh, mg) = remote_fixture(
        "api",
        r#"{
            "repos":[{"name":"api","services":{"mysql":{"port":3306}},
                      "scripts":{"up":"echo x","status":"docker compose ps"}}],
            "remote":{"connection":"srv","workspace_path":"ws"}
        }"#,
    );
    new_remote(&f, "api", &ssh, &mg);

    let args = f.path().join("list-ssh.txt");
    let out = f.run_env_ok(&["list"], &remote_env(&ssh, &mg, &args));

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
    let (f, ssh, mg) = remote_fixture("svc", host_repo_ws());
    new_remote(&f, "svc", &ssh, &mg);

    let args = f.path().join("up-ssh.txt");
    f.run_env_ok(&["up", "feat/foo"], &remote_env(&ssh, &mg, &args));

    let rec = std::fs::read_to_string(&args).expect("ssh was invoked");
    assert!(rec.contains("nohup "), "host process not launched (no nohup) over ssh:\n{rec}");
    assert!(
        rec.contains("cd /home/test/ws/feat-foo/svc"),
        "launched from the server worktree:\n{rec}"
    );
    assert!(rec.contains("ws-feat-foo-svc.pid"), "pid not recorded under ~/.ws-run:\n{rec}");
    assert!(rec.contains("WS_PORT_web="), "allocated port not injected:\n{rec}");
    assert!(rec.contains("sh -c 'run --port $WS_PORT_web'"), "up cmd not via sh -c:\n{rec}");
    // A host repo's toolchain comes from the user's login shell; without the
    // harvested PATH a `#!/usr/bin/env node` shebang dies at launch, in a log,
    // after the step has already reported green.
    assert!(
        rec.contains("PATH=/harvested/bin:/usr/bin"),
        "harvested login PATH not passed to the detached host process:\n{rec}"
    );
    // `.ws-hooks/…` script paths anchor against `WS_WORKSPACE_ROOT`, so handing
    // back the client's root points them at a filesystem the server doesn't have.
    assert!(
        rec.contains("WS_WORKSPACE_ROOT=/home/test/ws"),
        "WS_WORKSPACE_ROOT must be the server-side root for a detached host process:\n{rec}"
    );
}

#[test]
fn a_host_process_that_dies_on_launch_fails_the_step_and_shows_its_log() {
    // `nohup … &` always yields a pid, so the launch itself can never fail — the
    // launch script re-checks the process afterwards and returns its log tail.
    let f = WorkspaceFixture::empty();
    f.add_repo("svc");
    f.write_ws_json(host_repo_ws());
    // `__WS_HOST_DIED__` is what the real launch script emits when its re-check
    // finds the process already gone.
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
    let mg = f.stub("fake-mutagen", "exit 0");
    new_remote(&f, "svc", &ssh, &mg);

    let args = f.path().join("up-ssh.txt");
    let out = f.run_with_env(&["up", "feat/foo"], &remote_env(&ssh, &mg, &args));

    let err = stderr(&out);
    assert!(!out.status.success(), "a dead host process must fail `ws up`:\n{err}");
    assert!(
        err.contains("exited immediately"),
        "the step must say the process died, not report it started:\n{err}"
    );
    assert!(
        err.contains("No such file or directory"),
        "the log tail must be surfaced, not left in ~/.ws-run:\n{err}"
    );
}

#[test]
fn prepare_runs_on_the_client_while_up_dispatches_to_the_server() {
    // `prepare` is the one hook that stays on the client when offloading, so it can
    // stage local data (a db dump) into the worktree for the sync to carry.
    let (f, ssh, mg) = remote_fixture("api", remote_container_ws());
    f.add_hook("api", "prepare", "echo staged > \"$WS_WORKTREE/prepared.txt\"");
    new_remote(&f, "api", &ssh, &mg);

    let args = f.path().join("up-ssh.txt");
    f.run_env_ok(&["up", "feat/foo"], &remote_env(&ssh, &mg, &args));

    assert!(
        f.path().join(".worktrees/feat-foo/api/prepared.txt").exists(),
        "prepare must run on the client for a remote workspace"
    );
    let rec = std::fs::read_to_string(&args).expect("ssh was invoked");
    assert!(rec.contains("exec sh -c 'echo starting'"), "up must dispatch over ssh:\n{rec}");
}

#[test]
fn ws_hooks_are_pushed_to_the_server_before_a_remote_hook() {
    // A workspace-root `.ws-hooks/` script lives outside the repos, so it can't
    // ride the worktree sync and needs a push of its own.
    let (f, ssh, mg) = remote_fixture("api", remote_container_ws());
    std::fs::create_dir_all(f.path().join(".ws-hooks")).unwrap();
    std::fs::write(f.path().join(".ws-hooks/up.sh"), "#!/usr/bin/env bash\necho hi\n").unwrap();
    new_remote(&f, "api", &ssh, &mg);

    let args = f.path().join("up-ssh.txt");
    f.run_env_ok(&["up", "feat/foo"], &remote_env(&ssh, &mg, &args));

    let rec = std::fs::read_to_string(&args).expect("ssh was invoked");
    assert!(
        rec.lines().any(|l| l.contains("tar -C /home/test/ws") && l.contains("-xpf -")),
        "expected a .ws-hooks push (tar extract) over ssh:\n{rec}"
    );
    // `-xpf` preserves the mode, so a hook the client left as 644 lands unrunnable
    // and fails at hook time with a bare `Permission denied`.
    assert!(
        rec.lines().any(|l| l.contains("chmod +x") && l.contains("#!")),
        "expected the push to restore +x on shebang scripts:\n{rec}"
    );
}
