//! Remote-offload stack lifecycle: `ws up`/`ws down`/teardown for a `remote`
//! workspace must run the container lifecycle **on the server over SSH** (in the
//! synced worktree, with the injected env), not locally. These drive the dispatch
//! deterministically with a `WS_SSH` stub — the real end-to-end (podman + Caddy on
//! a real server) is exercised by the self-SSH-to-localhost smoke, which needs a
//! live box and so isn't a unit test.

mod common;

use common::{stderr, WorkspaceFixture};
use std::path::{Path, PathBuf};

/// A `WS_SSH` stub that: records every invocation's argv to `$ARGS_FILE`, answers
/// the `$HOME` probe (`Site::resolve`) with a fixed home, reports every candidate
/// port free (the `ss` probe expects a literal `free` on stdout), and reports a
/// host process as not-running (the pid-file probe's `kill -0` → non-zero) so
/// `supervise::up` proceeds to launch it. Everything else exits 0. Lets a full
/// remote `up`/`down` proceed without a real server.
fn ssh_stub(f: &WorkspaceFixture) -> PathBuf {
    f.stub(
        "fake-ssh",
        r#"printf '%s\n' "$@" >> "$ARGS_FILE"
last="${!#}"
case "$last" in
  *'printf %s'*) printf /home/test ;;
  *'ss -Hltn'*) echo free ;;
  *'kill -0'*) exit 3 ;;
  *'timeout '*) echo running ;;
  *) exit 0 ;;
esac"#,
    )
}

/// A no-op `WS_MUTAGEN` stub (mutagen isn't installed on CI; sync is best-effort).
fn mutagen_noop(f: &WorkspaceFixture) -> PathBuf {
    f.stub("fake-mutagen", "exit 0")
}

fn remote_container_ws() -> &'static str {
    // No `domain` ⇒ routing off, so `up` needs no Caddy — this isolates the
    // script-dispatch + port-allocation path. `mysql` is raw-TCP (gets a WS_PORT).
    r#"{
        "repos":[{"name":"api","services":{"mysql":{"port":3306}},
                  "scripts":{"up":"echo starting","down":"echo stopping"}}],
        "remote":{"connection":"srv","workspace_path":"ws"}
    }"#
}

/// Create the worktree (remote, so it resolves HOME + records the mysql port),
/// returning the fixture so a following `up`/`down` reuses the recorded allocation.
fn new_remote(f: &WorkspaceFixture, repo: &str, ssh: &Path, mg: &Path) {
    let out = f.run_with_env(
        &["new", repo, "feat/foo"],
        &[
            ("WS_SSH", ssh.to_str().unwrap()),
            ("WS_MUTAGEN", mg.to_str().unwrap()),
            ("ARGS_FILE", f.path().join("new-ssh.txt").to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "ws new failed:\n{}", stderr(&out));
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
            ("WS_SSH", ssh.to_str().unwrap()),
            ("WS_MUTAGEN", mg.to_str().unwrap()),
            ("ARGS_FILE", args.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "ws up failed:\n{}", stderr(&out));

    let rec = std::fs::read_to_string(&args).expect("ssh was invoked");
    // The up script runs in the synced server worktree, with the injected env, via
    // `sh -c` — one recorded ssh invocation carries the whole remote script.
    let line = rec
        .lines()
        .find(|l| l.contains("exec sh -c 'echo starting'"))
        .unwrap_or_else(|| panic!("up script not dispatched over ssh:\n{rec}"));
    // Absolute, home-resolved server dir — a relative base leaves
    // WS_WORKSPACE_ROOT relative and breaks `$WS_WORKSPACE_ROOT/…` in a hook.
    assert!(line.contains("cd /home/test/ws/feat-foo/api &&"), "wrong server dir:\n{line}");
    assert!(line.contains("WS_WORKSPACE_ROOT=/home/test/ws"), "WS_WORKSPACE_ROOT not absolute:\n{line}");
    assert!(line.contains("WS_REPO=api"), "missing WS_REPO:\n{line}");
    assert!(line.contains("WS_PORT_mysql="), "missing allocated port env:\n{line}");
    // The server-side hook must see it's the remote side and which runtime, so a
    // shared hook can branch (import a synced dump vs same-host copy; docker vs podman).
    assert!(line.contains("WS_REMOTE=1"), "missing WS_REMOTE=1 on the server:\n{line}");
    assert!(line.contains("WS_RUNTIME="), "missing WS_RUNTIME:\n{line}");
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
            ("WS_SSH", ssh.to_str().unwrap()),
            ("WS_MUTAGEN", mg.to_str().unwrap()),
            ("ARGS_FILE", args.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "ws down failed:\n{}", stderr(&out));

    let rec = std::fs::read_to_string(&args).expect("ssh was invoked");
    assert!(
        rec.lines().any(|l| l.contains("cd /home/test/ws/feat-foo/api &&")
            && l.contains("exec sh -c 'echo stopping'")),
        "down script not dispatched over ssh:\n{rec}"
    );
}

#[test]
fn list_runs_status_on_the_server_for_a_remote_workspace() {
    // `ws list` status for a remote workspace runs the declared `status` script ON
    // THE SERVER (in the synced worktree, timeout-bounded), not locally.
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
            ("WS_SSH", ssh.to_str().unwrap()),
            ("WS_MUTAGEN", mg.to_str().unwrap()),
            ("ARGS_FILE", args.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "ws list failed:\n{}", stderr(&out));

    let rec = std::fs::read_to_string(&args).expect("ssh was invoked");
    assert!(
        rec.lines().any(|l| l.contains("cd /home/test/ws/feat-foo/api &&")
            && l.contains("timeout 5 sh -c 'docker compose ps'")),
        "status not run on the server:\n{rec}"
    );
    // The status the server reported shows in the listing.
    let shown = format!("{}{}", common::stdout(&out), stderr(&out));
    assert!(shown.contains("running"), "status not surfaced in the listing:\n{shown}");
}

#[test]
fn host_repo_on_a_remote_workspace_runs_on_the_server() {
    // A `kind: host` repo on a remote workspace is supervised ON THE SERVER over
    // SSH: a detached `nohup … &` launched from the server worktree, with the
    // allocated port in its env and its pid recorded under `~/.ws-run`.
    let f = WorkspaceFixture::empty();
    f.add_repo("svc");
    f.write_ws_json(
        r#"{
            "repos":[{"name":"svc","kind":"host","services":{"web":{"port":8080}},
                      "scripts":{"up":"run --port $WS_PORT_web"}}],
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
            ("WS_SSH", ssh.to_str().unwrap()),
            ("WS_MUTAGEN", mg.to_str().unwrap()),
            ("ARGS_FILE", args.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "ws up failed:\n{}", stderr(&out));

    let rec = std::fs::read_to_string(&args).expect("ssh was invoked");
    assert!(rec.contains("nohup "), "host process not launched (no nohup) over ssh:\n{rec}");
    assert!(
        rec.contains("cd /home/test/ws/feat-foo/svc"),
        "launched from the server worktree:\n{rec}"
    );
    assert!(rec.contains("ws-feat-foo-svc.pid"), "pid not recorded under ~/.ws-run:\n{rec}");
    assert!(rec.contains("WS_PORT_web="), "allocated port not injected:\n{rec}");
    assert!(rec.contains("sh -c 'run --port $WS_PORT_web'"), "up cmd not via sh -c:\n{rec}");
}

#[test]
fn prepare_runs_on_the_client_while_up_dispatches_to_the_server() {
    // `prepare` is the one hook that stays on the client even when offloading, so
    // it can stage local data (a db dump) into the worktree for the sync to carry.
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(remote_container_ws());
    f.add_hook("api", "prepare", "echo staged > \"$WS_WORKTREE/prepared.txt\"");
    let ssh = ssh_stub(&f);
    let mg = mutagen_noop(&f);
    new_remote(&f, "api", &ssh, &mg);

    let args = f.path().join("up-ssh.txt");
    let out = f.run_with_env(
        &["up", "feat/foo"],
        &[
            ("WS_SSH", ssh.to_str().unwrap()),
            ("WS_MUTAGEN", mg.to_str().unwrap()),
            ("ARGS_FILE", args.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "ws up failed:\n{}", stderr(&out));

    // prepare wrote into the LOCAL worktree — it ran on this machine, not over ssh.
    assert!(
        f.path().join(".worktrees/feat-foo/api/prepared.txt").exists(),
        "prepare must run on the client for a remote workspace"
    );
    // ...while `up` itself was dispatched to the server over ssh.
    let rec = std::fs::read_to_string(&args).expect("ssh was invoked");
    assert!(rec.contains("exec sh -c 'echo starting'"), "up must dispatch over ssh:\n{rec}");
}

#[test]
fn ws_hooks_are_pushed_to_the_server_before_a_remote_hook() {
    // A workspace-root `.ws-hooks/` script lives OUTSIDE the repos, so it can't
    // ride the worktree sync — `ws` pushes the dir to the server so a hook can
    // reference `$WS_WORKSPACE_ROOT/.ws-hooks/…` there.
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(remote_container_ws());
    std::fs::create_dir_all(f.path().join(".ws-hooks")).unwrap();
    std::fs::write(f.path().join(".ws-hooks/up.sh"), "#!/usr/bin/env bash\necho hi\n").unwrap();
    let ssh = ssh_stub(&f);
    let mg = mutagen_noop(&f);
    new_remote(&f, "api", &ssh, &mg);

    let args = f.path().join("up-ssh.txt");
    let out = f.run_with_env(
        &["up", "feat/foo"],
        &[
            ("WS_SSH", ssh.to_str().unwrap()),
            ("WS_MUTAGEN", mg.to_str().unwrap()),
            ("ARGS_FILE", args.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "ws up failed:\n{}", stderr(&out));

    // The push is `ssh <conn> 'mkdir -p <base> && tar -C <base> -xpf -'`.
    let rec = std::fs::read_to_string(&args).expect("ssh was invoked");
    assert!(
        rec.lines().any(|l| l.contains("tar -C /home/test/ws -xpf -")),
        "expected a .ws-hooks push (tar extract) over ssh:\n{rec}"
    );
}
