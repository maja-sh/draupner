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
/// host unit as not-active (`is-active` → non-zero) so `supervise::up` proceeds to
/// `systemd-run`. Everything else exits 0. Lets a full remote `up`/`down` proceed
/// without a real server.
fn ssh_stub(f: &WorkspaceFixture) -> PathBuf {
    f.stub(
        "fake-ssh",
        r#"printf '%s\n' "$@" >> "$ARGS_FILE"
last="${!#}"
case "$last" in
  *'printf %s'*) printf /home/test ;;
  *'ss -Hltn'*) echo free ;;
  *'is-active'*) exit 3 ;;
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
    assert!(line.contains("cd ws/feat-foo/api &&"), "wrong server dir:\n{line}");
    assert!(line.contains("WS_REPO=api"), "missing WS_REPO:\n{line}");
    assert!(line.contains("WS_PORT_mysql="), "missing allocated port env:\n{line}");
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
        rec.lines().any(|l| l.contains("cd ws/feat-foo/api &&")
            && l.contains("exec sh -c 'echo stopping'")),
        "down script not dispatched over ssh:\n{rec}"
    );
}

#[test]
fn host_repo_on_a_remote_workspace_runs_systemd_on_the_server() {
    // A `kind: host` repo on a remote workspace is supervised as a `systemd --user`
    // unit ON THE SERVER over SSH: XDG_RUNTIME_DIR set (so the user bus is found),
    // the server worktree as --working-directory, the allocated port via --setenv.
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
    let line = rec
        .lines()
        .find(|l| l.contains("systemd-run --user"))
        .unwrap_or_else(|| panic!("systemd-run not dispatched over ssh:\n{rec}"));
    assert!(line.contains("export XDG_RUNTIME_DIR=/run/user/$(id -u)"), "XDG not set:\n{line}");
    assert!(line.contains("--unit=ws-feat-foo-svc"), "wrong unit:\n{line}");
    assert!(
        line.contains("--working-directory=/home/test/ws/feat-foo/svc"),
        "working dir must be the server worktree:\n{line}"
    );
    assert!(line.contains("WS_PORT_web="), "allocated port not injected:\n{line}");
    assert!(line.contains("sh -c 'run --port $WS_PORT_web'"), "up cmd not via sh -c:\n{line}");
}
