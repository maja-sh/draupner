mod common;

use common::{stderr, stdout, WorkspaceFixture};

// `ws config` needs no workspace — it's machine-global. The fixture is still
// used for its HOME isolation (`~/.config/ws` lands in the tempdir).

#[test]
fn config_set_get_unset_round_trip() {
    let ws = WorkspaceFixture::empty();

    ws.run_ok(&["config", "set", "remote.host", "srv"]);
    let file = ws.path().join(".config/ws/config.toml");
    let raw = std::fs::read_to_string(&file).expect("config.toml written");
    assert!(raw.contains("[remote]") && raw.contains("host = \"srv\""), "{raw}");

    let out = ws.run_ok(&["config", "get", "remote.host"]);
    assert_eq!(stdout(&out), "srv\n", "bare value on stdout");

    ws.run_ok(&["config", "unset", "remote.host"]);
    let out = ws.run(&["config", "get", "remote.host"]);
    assert_eq!(out.status.code(), Some(1), "unset key → exit 1 (git parity)");
    assert_eq!(stdout(&out), "", "nothing on stdout for an unset key");
    assert!(stderr(&out).contains("not set"), "human-facing note on stderr");

    // The emptied section is pruned, not left as a bare `[remote]` header.
    let raw = std::fs::read_to_string(&file).unwrap();
    assert_eq!(raw.trim(), "", "file empties out cleanly: {raw}");
}

#[test]
fn config_unset_of_unset_key_is_a_quiet_no_op() {
    let ws = WorkspaceFixture::empty();
    ws.run_ok(&["config", "unset", "remote.host"]);
    // No file materialized just to record nothing.
    assert!(!ws.path().join(".config/ws/config.toml").exists());
}

#[test]
fn config_rejects_unknown_keys() {
    let ws = WorkspaceFixture::empty();
    for args in [
        &["config", "get", "remote.hots"][..],
        &["config", "set", "no.such.key", "x"][..],
        &["config", "unset", "remote"][..],
    ] {
        let out = ws.run(args);
        assert_eq!(out.status.code(), Some(2), "unknown key → preflight: {args:?}");
        assert!(stderr(&out).contains("known keys"), "lists known keys:\n{}", stderr(&out));
    }
}

#[test]
fn config_rejects_an_option_shaped_host() {
    let ws = WorkspaceFixture::empty();
    // Without `--`, clap already refuses the hyphen-leading value; `--` gets it
    // past clap so ws's own guard is what's exercised.
    let out = ws.run(&["config", "set", "--", "remote.host", "-oProxyCommand=evil"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("unsafe SSH host"), "{}", stderr(&out));
    assert!(!ws.path().join(".config/ws/config.toml").exists(), "nothing persisted");
}

#[test]
fn config_get_reports_a_malformed_file_loudly() {
    let ws = WorkspaceFixture::empty();
    let dir = ws.path().join(".config/ws");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), "not [valid toml").unwrap();

    let out = ws.run(&["config", "get", "remote.host"]);
    assert!(!out.status.success(), "malformed file must not read as empty");
    assert!(stderr(&out).contains("config.toml"), "names the file:\n{}", stderr(&out));
}

#[test]
fn init_remote_falls_back_to_the_global_default_host() {
    let ws = WorkspaceFixture::empty();
    ws.add_repo("api");
    let ok = ws.stub("ok", "exit 0");
    ws.run_ok(&["config", "set", "remote.host", "global-host"]);

    // Bare `--remote`, no $WS_REMOTE_HOST → the global default fills in.
    let out = ws.run_with_env(
        &["init", "--remote"],
        &[("WS_SSH", ok.to_str().unwrap()), ("WS_MUTAGEN", ok.to_str().unwrap())],
    );
    assert!(out.status.success(), "init should use the global default:\n{}", stderr(&out));
    let cfg = std::fs::read_to_string(ws.path().join(".ws.json")).unwrap();
    assert!(cfg.contains("global-host"), "global default host expected:\n{cfg}");
}

#[test]
fn env_beats_the_global_default_and_flag_beats_env() {
    let ws = WorkspaceFixture::empty();
    ws.add_repo("api");
    let ok = ws.stub("ok", "exit 0");
    ws.run_ok(&["config", "set", "remote.host", "global-host"]);

    // env > global.
    let out = ws.run_with_env(
        &["init", "--remote"],
        &[
            ("WS_REMOTE_HOST", "env-host"),
            ("WS_SSH", ok.to_str().unwrap()),
            ("WS_MUTAGEN", ok.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let cfg = std::fs::read_to_string(ws.path().join(".ws.json")).unwrap();
    assert!(cfg.contains("env-host") && !cfg.contains("global-host"), "env wins:\n{cfg}");

    // flag > env (fresh workspace — init refuses to rerun).
    let ws2 = WorkspaceFixture::empty();
    ws2.add_repo("api");
    let ok2 = ws2.stub("ok", "exit 0");
    let out = ws2.run_with_env(
        &["init", "--remote", "flag-host"],
        &[
            ("WS_REMOTE_HOST", "env-host"),
            ("WS_SSH", ok2.to_str().unwrap()),
            ("WS_MUTAGEN", ok2.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let cfg = std::fs::read_to_string(ws2.path().join(".ws.json")).unwrap();
    assert!(cfg.contains("flag-host") && !cfg.contains("env-host"), "flag wins:\n{cfg}");
}

#[test]
fn init_remote_without_any_host_names_all_three_sources() {
    let ws = WorkspaceFixture::empty();
    ws.add_repo("api");
    let out = ws.run(&["init", "--remote"]);
    assert_eq!(out.status.code(), Some(2));
    let err = stderr(&out);
    for hint in ["--remote <host>", "WS_REMOTE_HOST", "ws config set remote.host"] {
        assert!(err.contains(hint), "error should mention `{hint}`:\n{err}");
    }
}
