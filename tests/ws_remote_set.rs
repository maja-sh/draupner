mod common;

use common::{stderr, WorkspaceFixture};

// `ws remote set` / `ws remote unset` — flipping a workspace local↔remote
// after init. The mutagen/ssh binaries are stubbed (`WS_MUTAGEN`/`WS_SSH`), so
// these assert the exact sync lifecycle `ws` drives, not real syncing.

fn recorded(args_file: &std::path::Path) -> String {
    std::fs::read_to_string(args_file).unwrap_or_default()
}

#[test]
fn remote_set_writes_the_block_and_backfills_sync_for_existing_worktrees() {
    let ws = WorkspaceFixture::with_repos(&["api", "web"]);
    ws.run_ok(&["new", "api,web", "feat/x"]);

    let ok = ws.stub("ok", "exit 0");
    let rec = ws.arg_recorder("mutagen-rec");
    let args_file = ws.path().join("mutagen-args");

    let out = ws.run_with_env(
        &["remote", "set", "srv"],
        &[
            ("WS_SSH", ok.to_str().unwrap()),
            ("WS_MUTAGEN", rec.to_str().unwrap()),
            ("ARGS_FILE", args_file.to_str().unwrap()),
        ],
    );
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

    let ok = ws.stub("ok", "exit 0");
    let rec = ws.arg_recorder("mutagen-rec");
    let args_file = ws.path().join("mutagen-args");

    let out = ws.run_with_env(
        &["remote", "set", "srv"],
        &[
            ("WS_SSH", ok.to_str().unwrap()),
            ("WS_MUTAGEN", rec.to_str().unwrap()),
            ("ARGS_FILE", args_file.to_str().unwrap()),
        ],
    );
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

    let ok = ws.stub("ok", "exit 0");
    let rec = ws.arg_recorder("mutagen-rec");
    let args_file = ws.path().join("mutagen-args");

    let out = ws.run_with_env(
        &["remote", "set", "srv"],
        &[
            ("WS_SSH", ok.to_str().unwrap()),
            ("WS_MUTAGEN", rec.to_str().unwrap()),
            ("ARGS_FILE", args_file.to_str().unwrap()),
        ],
    );
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

    let ok = ws.stub("ok", "exit 0");
    // Records every call; fails `sync terminate` with an error that is NOT one
    // of the "already gone" patterns (a real daemon failure).
    let rec = ws.stub(
        "mutagen-flaky",
        "printf '%s\\n' \"$@\" >> \"$ARGS_FILE\"\n\
         if [ \"$2\" = terminate ]; then echo 'daemon connection lost' >&2; exit 1; fi",
    );
    let args_file = ws.path().join("mutagen-args");

    let out = ws.run_with_env(
        &["remote", "set", "srv"],
        &[
            ("WS_SSH", ok.to_str().unwrap()),
            ("WS_MUTAGEN", rec.to_str().unwrap()),
            ("ARGS_FILE", args_file.to_str().unwrap()),
        ],
    );
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

    let ok = ws.stub("ok", "exit 0");
    let rec = ws.arg_recorder("mutagen-rec");
    let args_file = ws.path().join("mutagen-args");

    let out = ws.run_with_env(
        &["remote", "set", "new-host"],
        &[
            ("WS_SSH", ok.to_str().unwrap()),
            ("WS_MUTAGEN", rec.to_str().unwrap()),
            ("ARGS_FILE", args_file.to_str().unwrap()),
        ],
    );
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

#[test]
fn remote_unset_on_a_local_workspace_is_a_quiet_no_op() {
    let ws = WorkspaceFixture::with_repos(&["api"]);
    let before = std::fs::read_to_string(ws.path().join(".ws.json")).unwrap();
    let out = ws.run_ok(&["remote", "unset"]);
    assert!(stderr(&out).contains("already local"), "{}", stderr(&out));
    let after = std::fs::read_to_string(ws.path().join(".ws.json")).unwrap();
    assert_eq!(before, after, "no rewrite for a no-op");
}
