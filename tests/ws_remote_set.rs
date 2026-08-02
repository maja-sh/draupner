mod common;

use common::{stderr, WorkspaceFixture};
use std::path::{Path, PathBuf};
use std::process::Output;

// `ws remote set` / `ws remote unset` — flipping a workspace local↔remote
// after init. The mutagen/ssh binaries are stubbed (`WS_MUTAGEN`/`WS_SSH`), so
// these assert the exact sync lifecycle `ws` drives, not real syncing.

fn recorded(args_file: &Path) -> String {
    std::fs::read_to_string(args_file).unwrap_or_default()
}

/// Run `ws <argv>` with ssh stubbed reachable and `WS_MUTAGEN` pointed at
/// `mutagen` (usually an arg recorder). Returns the output and the file the stub
/// records its argv to.
fn run_with_stubs(ws: &WorkspaceFixture, argv: &[&str], mutagen: &Path) -> (Output, PathBuf) {
    let ok = ws.stub("ok", "exit 0");
    let args_file = ws.path().join("mutagen-args");
    let out = ws.run_with_env(
        argv,
        &[
            ("WS_SSH", ok.to_str().unwrap()),
            ("WS_MUTAGEN", mutagen.to_str().unwrap()),
            ("ARGS_FILE", args_file.to_str().unwrap()),
        ],
    );
    (out, args_file)
}

#[test]
fn remote_set_writes_the_block_and_backfills_sync_for_existing_worktrees() {
    let ws = WorkspaceFixture::with_repos(&["api", "web"]);
    ws.run_ok(&["new", "api,web", "feat/x"]);

    let (out, args_file) =
        run_with_stubs(&ws, &["remote", "set", "srv"], &ws.arg_recorder("mutagen-rec"));
    assert!(out.status.success(), "remote set failed:\n{}", stderr(&out));

    let cfg = std::fs::read_to_string(ws.path().join(".ws.json")).unwrap();
    assert!(cfg.contains("\"connection\": \"srv\""), "remote block written:\n{cfg}");

    // Both pre-existing worktrees got a session against the new endpoint, then
    // an initial flush.
    let calls = recorded(&args_file);
    let base = ws.path().file_name().unwrap().to_str().unwrap();
    for repo in ["api", "web"] {
        assert!(calls.contains(&format!("ws-feat-x-{repo}")), "session for {repo}:\n{calls}");
        assert!(
            calls.contains(&format!("srv:{base}/feat-x/{repo}")),
            "beta endpoint for {repo}:\n{calls}"
        );
    }
    assert!(calls.contains("create"), "sync create expected:\n{calls}");
    assert!(calls.contains("flush"), "initial flush expected:\n{calls}");
}

#[test]
fn remote_set_ignores_dep_artifacts_present_in_the_worktree() {
    let ws = WorkspaceFixture::with_repos(&["api"]);
    ws.run_ok(&["new", "api", "feat/x"]);
    // The existing worktree is ground truth: a vendor/ dir present at flip time
    // must stay off the wire, same as `ws new` would have arranged.
    std::fs::create_dir_all(ws.path().join(".worktrees/feat-x/api/vendor")).unwrap();

    let (out, args_file) =
        run_with_stubs(&ws, &["remote", "set", "srv"], &ws.arg_recorder("mutagen-rec"));
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        recorded(&args_file).contains("--ignore=/vendor"),
        "vendor/ must be a sync ignore:\n{}",
        recorded(&args_file)
    );
}

#[test]
fn remote_set_never_ignores_a_git_tracked_vendor_dir() {
    let ws = WorkspaceFixture::with_repos(&["api"]);
    // Go-style committed vendoring: vendor/ is tracked source, not a per-host
    // artifact — it must reach the server, so it must NOT become a sync ignore.
    std::fs::create_dir_all(ws.path().join("api/vendor")).unwrap();
    std::fs::write(ws.path().join("api/vendor/lib.go"), "package lib").unwrap();
    ws.git("api", &["add", "vendor"]);
    ws.git("api", &["commit", "-q", "-m", "vendor deps"]);
    ws.run_ok(&["new", "api", "feat/x"]);

    let (out, args_file) =
        run_with_stubs(&ws, &["remote", "set", "srv"], &ws.arg_recorder("mutagen-rec"));
    assert!(out.status.success(), "{}", stderr(&out));
    let calls = recorded(&args_file);
    assert!(calls.contains("create"), "sync still created:\n{calls}");
    assert!(
        !calls.contains("--ignore=/vendor"),
        "tracked vendor/ is committed source and must stay on the wire:\n{calls}"
    );
}

#[test]
fn remote_set_fails_the_worktree_when_a_stale_session_wont_terminate() {
    let ws = WorkspaceFixture::with_repos(&["api"]);
    ws.run_ok(&["new", "api", "feat/x"]);

    // Records every call; fails `sync terminate` with an error that is NOT one
    // of the "already gone" patterns (a real daemon failure).
    let rec = ws.stub(
        "mutagen-flaky",
        "printf '%s\\n' \"$@\" >> \"$ARGS_FILE\"\n\
         if [ \"$2\" = terminate ]; then echo 'daemon connection lost' >&2; exit 1; fi",
    );

    let (out, args_file) = run_with_stubs(&ws, &["remote", "set", "srv"], &rec);
    // A create after a failed terminate would no-op on the name collision and
    // report success while the session syncs to the wrong endpoint — so the
    // worktree must FAIL and the create must not run.
    assert_eq!(out.status.code(), Some(1), "must fail:\n{}", stderr(&out));
    assert!(stderr(&out).contains("stale session"), "{}", stderr(&out));
    assert!(!recorded(&args_file).contains("create"), "create must be skipped:\n{}", recorded(&args_file));
}

#[test]
fn remote_set_treats_an_empty_path_as_absent() {
    let ws = WorkspaceFixture::with_repos(&["api"]);
    // `--path "$VAR"` with an unset VAR must not anchor the server layout at
    // the filesystem root — it falls back to the workspace dir-name default.
    let out = ws.run(&["remote", "set", "srv", "--path", "", "--no-verify"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let cfg = std::fs::read_to_string(ws.path().join(".ws.json")).unwrap();
    let base = ws.path().file_name().unwrap().to_str().unwrap();
    assert!(!cfg.contains("\"workspace_path\": \"\""), "empty path must not be stored:\n{cfg}");
    assert!(cfg.contains(&format!("\"workspace_path\": \"{base}\"")), "{cfg}");
}

#[test]
fn remote_set_repoints_an_existing_remote_by_recreating_sessions() {
    let ws = WorkspaceFixture::with_repos(&["api"]);
    ws.run_ok(&["new", "api", "feat/x"]);
    // Simulate a workspace already offloaded to old-host (worktree was created
    // while local so no server is needed).
    ws.write_ws_json(
        r#"{"repos":["api"],"remote":{"connection":"old-host","workspace_path":"proj"}}"#,
    );

    let (out, args_file) =
        run_with_stubs(&ws, &["remote", "set", "new-host"], &ws.arg_recorder("mutagen-rec"));
    assert!(out.status.success(), "{}", stderr(&out));

    // The old-endpoint session dies before the new one is created — `sync
    // create` alone would no-op on the name collision and keep syncing to
    // old-host.
    let calls = recorded(&args_file);
    let terminate = calls.find("terminate").expect("stale session terminated");
    let create = calls.find("create").expect("session recreated");
    assert!(terminate < create, "terminate must precede create:\n{calls}");
    assert!(calls.contains("new-host:proj/feat-x/api"), "new endpoint:\n{calls}");

    // --path was not given → the configured workspace_path is kept.
    let cfg = std::fs::read_to_string(ws.path().join(".ws.json")).unwrap();
    assert!(cfg.contains("\"workspace_path\": \"proj\""), "path kept on re-point:\n{cfg}");
    assert!(cfg.contains("new-host"), "{cfg}");
}

#[test]
fn remote_set_terminates_the_old_hosts_port_forwards_on_a_repoint() {
    let ws = WorkspaceFixture::with_repos(&["api"]);
    ws.run_ok(&["new", "api", "feat/x"]);
    ws.write_ws_json(
        r#"{"repos":["api"],"remote":{"connection":"old-host","workspace_path":"proj"}}"#,
    );
    // A previous `ws up` against old-host allocated a port and left a forward
    // session for it. The forward's endpoint is fixed at create time but its name
    // isn't host-keyed, so re-pointing must terminate it — otherwise the next `ws
    // up` no-ops on the name collision and `localhost:20000` still reaches
    // old-host while the port-forwards table claims the new one.
    std::fs::create_dir_all(ws.path().join(".ws-meta")).unwrap();
    std::fs::write(ws.path().join(".ws-meta/feat-x-api.json"), r#"{"ports":{"db":20000}}"#).unwrap();

    let (out, args_file) =
        run_with_stubs(&ws, &["remote", "set", "new-host"], &ws.arg_recorder("mutagen-rec"));
    assert!(out.status.success(), "{}", stderr(&out));

    // The recorder writes one argv per line, so this is the exact
    // `mutagen forward terminate ws-fwd-feat-x-api-20000` invocation.
    let calls = recorded(&args_file);
    assert!(
        calls.contains("forward\nterminate\nws-fwd-feat-x-api-20000"),
        "the old host's forward must be terminated on a re-point:\n{calls}"
    );
    // The sync backfill still runs alongside it.
    assert!(calls.contains("create"), "sync still recreated:\n{calls}");
}

#[test]
fn remote_set_writes_domain_and_preserves_extended_repo_config() {
    let ws = WorkspaceFixture::with_repos(&["api"]);
    ws.write_ws_json(
        r#"{"repos":[{"name":"api","kind":"host","services":{"web":{"http":true,"port":8080}},"scripts":{"up":"serve"},"env":{"APP_URL":"https://${WS_HOST_web}"}}]}"#,
    );

    // No worktrees yet → no sync backfill, so no stubs needed beyond skipping
    // the reachability check.
    let out = ws.run(&["remote", "set", "srv", "--domain", "myapp.test", "--no-verify"]);
    assert!(out.status.success(), "{}", stderr(&out));

    let cfg = std::fs::read_to_string(ws.path().join(".ws.json")).unwrap();
    assert!(cfg.contains("\"domain\": \"myapp.test\""), "top-level domain written:\n{cfg}");
    // The rewrite must not shed the extended repo config it round-tripped.
    for kept in ["\"kind\": \"host\"", "\"http\": true", "\"up\": \"serve\"", "WS_HOST_web"] {
        assert!(cfg.contains(kept), "extended config lost `{kept}`:\n{cfg}");
    }
}

#[test]
fn remote_set_refuses_an_unreachable_host_and_leaves_config_untouched() {
    let ws = WorkspaceFixture::with_repos(&["api"]);
    let before = std::fs::read_to_string(ws.path().join(".ws.json")).unwrap();

    let ok = ws.stub("ok", "exit 0");
    let bad = ws.stub("bad", "exit 1");
    let out = ws.run_with_env(
        &["remote", "set", "srv"],
        &[("WS_MUTAGEN", ok.to_str().unwrap()), ("WS_SSH", bad.to_str().unwrap())],
    );
    assert_eq!(out.status.code(), Some(2), "should refuse:\n{}", stderr(&out));
    assert!(stderr(&out).contains("reach"), "{}", stderr(&out));
    let after = std::fs::read_to_string(ws.path().join(".ws.json")).unwrap();
    assert_eq!(before, after, "refusal must leave .ws.json untouched");
}

#[test]
fn remote_set_rejects_an_option_shaped_host() {
    let ws = WorkspaceFixture::with_repos(&["api"]);
    let out = ws.run(&["remote", "set", "--", "-oProxyCommand=evil"]);
    // Whether clap or the guard catches it, an option-shaped host must never
    // reach ssh argv.
    assert!(!out.status.success());
    assert!(!std::fs::read_to_string(ws.path().join(".ws.json")).unwrap().contains("ProxyCommand"));
}

#[test]
fn remote_set_falls_back_to_env_then_global_config_for_the_host() {
    let ws = WorkspaceFixture::with_repos(&["api"]);
    ws.run_ok(&["config", "set", "remote.host", "global-host"]);

    let out = ws.run(&["remote", "set", "--no-verify"]);
    assert!(out.status.success(), "global fallback:\n{}", stderr(&out));
    let cfg = std::fs::read_to_string(ws.path().join(".ws.json")).unwrap();
    assert!(cfg.contains("global-host"), "{cfg}");

    // env beats global on the next re-point.
    let out = ws.run_with_env(&["remote", "set", "--no-verify"], &[("WS_REMOTE_HOST", "env-host")]);
    assert!(out.status.success(), "{}", stderr(&out));
    let cfg = std::fs::read_to_string(ws.path().join(".ws.json")).unwrap();
    assert!(cfg.contains("env-host") && !cfg.contains("global-host"), "env wins:\n{cfg}");
}

#[test]
fn remote_set_without_any_host_names_all_three_sources() {
    let ws = WorkspaceFixture::with_repos(&["api"]);
    let out = ws.run(&["remote", "set"]);
    assert_eq!(out.status.code(), Some(2));
    let err = stderr(&out);
    for hint in ["WS_REMOTE_HOST", "ws config set remote.host"] {
        assert!(err.contains(hint), "error should mention `{hint}`:\n{err}");
    }
}

#[test]
fn remote_unset_terminates_sync_and_removes_the_block() {
    let ws = WorkspaceFixture::with_repos(&["api"]);
    ws.run_ok(&["new", "api", "feat/x"]);
    ws.write_ws_json(r#"{"repos":["api"],"remote":{"connection":"srv"}}"#);

    let rec = ws.arg_recorder("mutagen-rec");
    let args_file = ws.path().join("mutagen-args");
    let out = ws.run_with_env(
        &["remote", "unset"],
        &[
            ("WS_MUTAGEN", rec.to_str().unwrap()),
            ("ARGS_FILE", args_file.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));

    let calls = recorded(&args_file);
    assert!(
        calls.contains("terminate") && calls.contains("ws-feat-x-api"),
        "session terminated:\n{calls}"
    );
    let cfg = std::fs::read_to_string(ws.path().join(".ws.json")).unwrap();
    assert!(!cfg.contains("remote"), "remote block removed:\n{cfg}");
}

// --- the flip is refused while a stack is running -------------------------
//
// Flipping a live workspace strands it: after `set` a locally-running stack is
// beyond `ws down` (every op dispatches to the server from then on); after
// `unset` the server's containers and routes are equally out of reach. These
// assert the pre-flight that enforces stop → flip → start instead.

/// A `kind: host` repo whose `up` is a long sleep — a genuinely running,
/// `ws`-supervised process for the pre-flight to find. `$HOME` is the fixture, so
/// the pid file under `~/.ws-run` is isolated.
fn running_host_workspace() -> WorkspaceFixture {
    let ws = WorkspaceFixture::empty();
    ws.add_repo("api");
    ws.write_ws_json(r#"{"repos":[{"name":"api","kind":"host","scripts":{"up":"exec sleep 40"}}]}"#);
    ws.run_ok(&["new", "api", "feat/x"]);
    ws.run_ok(&["up", "feat/x"]);
    ws
}

/// Kill the supervised process directly, whatever state the config ended in —
/// `ws rm` would dispatch to a server the test never had.
fn kill_host_proc(ws: &WorkspaceFixture) {
    let Ok(contents) = std::fs::read_to_string(ws.path().join(".ws-run/ws-feat-x-api.pid")) else {
        return;
    };
    if let Some(pid) = contents.lines().next() {
        let _ = std::process::Command::new("kill").args(["-9", pid.trim()]).status();
    }
}

#[test]
fn remote_set_refuses_while_a_stack_is_running() {
    let ws = running_host_workspace();

    let out = ws.run(&["remote", "set", "srv", "--no-verify"]);
    kill_host_proc(&ws);

    assert_eq!(out.status.code(), Some(2), "must refuse:\n{}", stderr(&out));
    let err = stderr(&out);
    // Name what's up and the exact command that clears it.
    assert!(err.contains("still running: feat/x (api)"), "{err}");
    assert!(err.contains("`ws down feat/x`"), "{err}");
    assert!(err.contains("--force"), "the override must be discoverable:\n{err}");
    // A refusal must not have written the block it refused to act on.
    let cfg = std::fs::read_to_string(ws.path().join(".ws.json")).unwrap();
    assert!(!cfg.contains("srv"), "refusal must leave .ws.json untouched:\n{cfg}");
}

#[test]
fn remote_set_force_flips_past_a_running_stack() {
    let ws = running_host_workspace();
    let ok = ws.stub("ok", "exit 0");
    let rec = ws.arg_recorder("mutagen-rec");
    let args_file = ws.path().join("mutagen-args");

    let out = ws.run_with_env(
        &["remote", "set", "srv", "--no-verify", "--force"],
        &[
            ("WS_SSH", ok.to_str().unwrap()),
            ("WS_MUTAGEN", rec.to_str().unwrap()),
            ("ARGS_FILE", args_file.to_str().unwrap()),
        ],
    );
    kill_host_proc(&ws);

    assert!(out.status.success(), "--force must flip anyway:\n{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("--force"), "the override should say what it carried past:\n{err}");
    // The recovery sequence is printed precisely when something really was left
    // behind — it's noise on a flip that had nothing running.
    assert!(err.contains("ws remote unset"), "recovery sequence expected:\n{err}");
    let cfg = std::fs::read_to_string(ws.path().join(".ws.json")).unwrap();
    assert!(cfg.contains("\"connection\": \"srv\""), "{cfg}");
}

#[test]
fn remote_set_refuses_while_a_container_stack_is_running() {
    let ws = WorkspaceFixture::with_repos(&["api"]);
    ws.run_ok(&["new", "api", "feat/x"]);
    // A compose project in the worktree, plus a runtime stub reporting one
    // container — the `compose ps -q` half of the pre-flight.
    std::fs::write(
        ws.path().join(".worktrees/feat-x/api/docker-compose.yml"),
        "services: {}\n",
    )
    .unwrap();
    let runtime = ws.stub(
        "runtime-stub",
        "if [ \"$1\" = compose ] && [ \"$2\" = ps ]; then echo 9f1c0deadbee; fi\nexit 0",
    );
    ws.write_ws_json(&format!(
        r#"{{"repos":["api"],"runtime":"{}"}}"#,
        runtime.display()
    ));

    let out = ws.run(&["remote", "set", "srv", "--no-verify"]);
    assert_eq!(out.status.code(), Some(2), "must refuse:\n{}", stderr(&out));
    assert!(stderr(&out).contains("still running: feat/x (api)"), "{}", stderr(&out));
}

#[test]
fn remote_set_proceeds_when_the_container_stack_is_down() {
    let ws = WorkspaceFixture::with_repos(&["api"]);
    ws.run_ok(&["new", "api", "feat/x"]);
    std::fs::write(
        ws.path().join(".worktrees/feat-x/api/docker-compose.yml"),
        "services: {}\n",
    )
    .unwrap();
    // Same shape, but `compose ps -q` reports nothing running.
    let runtime = ws.stub("runtime-stub", "exit 0");
    ws.write_ws_json(&format!(
        r#"{{"repos":["api"],"runtime":"{}"}}"#,
        runtime.display()
    ));

    let ok = ws.stub("ok", "exit 0");
    let rec = ws.arg_recorder("mutagen-rec");
    let args_file = ws.path().join("mutagen-args");
    let out = ws.run_with_env(
        &["remote", "set", "srv", "--no-verify"],
        &[
            ("WS_SSH", ok.to_str().unwrap()),
            ("WS_MUTAGEN", rec.to_str().unwrap()),
            ("ARGS_FILE", args_file.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "an idle stack must not block the flip:\n{}", stderr(&out));
    // And nothing was stranded, so the recovery note stays out of the way.
    assert!(
        !stderr(&out).contains("ws remote unset"),
        "recovery note is for stranded stacks only:\n{}",
        stderr(&out)
    );
}

#[test]
fn remote_unset_refuses_when_it_cannot_tell_what_is_running() {
    let ws = WorkspaceFixture::with_repos(&["api"]);
    ws.run_ok(&["new", "api", "feat/x"]);
    std::fs::write(
        ws.path().join(".worktrees/feat-x/api/docker-compose.yml"),
        "services: {}\n",
    )
    .unwrap();
    ws.write_ws_json(r#"{"repos":["api"],"remote":{"connection":"srv"}}"#);

    // The server is unreachable, so the stack there is unknowable. Reading that
    // as "nothing is running" is exactly how a stack gets stranded.
    let bad = ws.stub("bad", "exit 255");
    let out = ws.run_with_env(&["remote", "unset"], &[("WS_SSH", bad.to_str().unwrap())]);
    assert_eq!(out.status.code(), Some(2), "must refuse:\n{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("can't tell whether anything is still running"), "{err}");
    assert!(err.contains("isn't reachable over SSH"), "{err}");
    let cfg = std::fs::read_to_string(ws.path().join(".ws.json")).unwrap();
    assert!(cfg.contains("srv"), "refusal must leave the remote block in place:\n{cfg}");
}

#[test]
fn remote_unset_force_flips_when_liveness_is_unknown() {
    let ws = WorkspaceFixture::with_repos(&["api"]);
    ws.run_ok(&["new", "api", "feat/x"]);
    std::fs::write(
        ws.path().join(".worktrees/feat-x/api/docker-compose.yml"),
        "services: {}\n",
    )
    .unwrap();
    ws.write_ws_json(r#"{"repos":["api"],"remote":{"connection":"srv"}}"#);

    let bad = ws.stub("bad", "exit 255");
    let rec = ws.arg_recorder("mutagen-rec");
    let args_file = ws.path().join("mutagen-args");
    let out = ws.run_with_env(
        &["remote", "unset", "--force"],
        &[
            ("WS_SSH", bad.to_str().unwrap()),
            ("WS_MUTAGEN", rec.to_str().unwrap()),
            ("ARGS_FILE", args_file.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "--force must go local anyway:\n{}", stderr(&out));
    let cfg = std::fs::read_to_string(ws.path().join(".ws.json")).unwrap();
    assert!(!cfg.contains("remote"), "remote block removed:\n{cfg}");
    // Unknown liveness counts as possibly-stranded, so the recovery route is shown.
    assert!(stderr(&out).contains("ws remote set srv"), "{}", stderr(&out));
}

// --- the flip invalidates the wildcard's correct answer --------------------

#[test]
fn remote_set_says_the_wildcard_answer_is_now_stale() {
    let ws = WorkspaceFixture::with_repos(&["api"]);
    ws.write_ws_json(r#"{"repos":["api"],"domain":"myapp.test"}"#);
    // `ssh -G` (config parsing only, never a connection) is how the client's
    // address for the server is derived.
    let sshg = ws.stub(
        "ssh-g",
        "for a in \"$@\"; do\n  if [ \"$a\" = -G ]; then echo 'hostname 127.0.0.2'; exit 0; fi\ndone\nexit 0",
    );

    let out = ws.run_with_env(
        &["remote", "set", "srv", "--no-verify"],
        &[("WS_SSH", sshg.to_str().unwrap())],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("`*.myapp.test`"), "{err}");
    assert!(err.contains("should resolve to 127.0.0.2, not 127.0.0.1"), "{err}");
    // A nudge, not an action: rewriting a resolver stays user-consented.
    assert!(err.contains("ws setup"), "{err}");
    assert!(err.contains("never edits your resolver"), "{err}");
    // On a tty the flip offers `ws setup`'s DNS step instead of this note. Here
    // stdin isn't one, so nothing is offered: no prompt that can't be answered,
    // no command block, and above all no wait for a keypress that never comes.
    assert!(!err.contains("[R]un"), "no unanswerable prompt in a scripted flip: {err}");
    assert!(!err.contains("dnsmasq"), "no command block in a scripted flip: {err}");
}

#[test]
fn remote_unset_says_the_wildcard_should_point_back_at_loopback() {
    let ws = WorkspaceFixture::with_repos(&["api"]);
    // The incident's exact shape: the wildcard was pointed at the offload
    // server's address, and going local leaves it aimed at a machine this
    // workspace no longer uses.
    ws.write_ws_json(
        r#"{"repos":["api"],"domain":"myapp.test",
            "remote":{"connection":"srv","publish_address":"100.97.195.105"}}"#,
    );
    let rec = ws.arg_recorder("mutagen-rec");
    let args_file = ws.path().join("mutagen-args");

    let out = ws.run_with_env(
        &["remote", "unset"],
        &[
            ("WS_MUTAGEN", rec.to_str().unwrap()),
            ("ARGS_FILE", args_file.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(
        err.contains("should resolve to 127.0.0.1, not 100.97.195.105"),
        "{err}"
    );
    assert!(err.contains("ws setup"), "{err}");
    assert!(!err.contains("[R]un"), "no unanswerable prompt in a scripted flip: {err}");
}

#[test]
fn remote_unset_on_a_local_workspace_is_a_quiet_no_op() {
    let ws = WorkspaceFixture::with_repos(&["api"]);
    let before = std::fs::read_to_string(ws.path().join(".ws.json")).unwrap();
    let out = ws.run_ok(&["remote", "unset"]);
    assert!(stderr(&out).contains("already local"), "{}", stderr(&out));
    let after = std::fs::read_to_string(ws.path().join(".ws.json")).unwrap();
    assert_eq!(before, after, "no rewrite for a no-op");
}
