//! The contract after removing the `.drup-hooks/` anchor: a script path passes through verbatim, the shell cwd is the worktree, and workspace-level files need the explicit `$DRUP_WORKSPACE_ROOT/` prefix. These three tests pin the new behavior end-to-end so a future refactor can't silently revert it.

mod common;

use common::{stderr, write_exec, WorkspaceFixture};

/// The common migration shape: a pre-0.13 config has `scripts: {"up": ".drup-hooks/x.sh"}` with the file sitting at workspace root — the anchor used to resolve it. The preflight at config load refuses it with the exact JSON to paste, before any op reaches a cryptic `sh: .drup-hooks/x.sh: not found` at run time.
#[test]
fn a_pre_013_anchor_reference_is_refused_at_config_load_with_the_fix_to_paste() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    let hook = f.path().join(".drup-hooks").join("api-up.sh");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    write_exec(&hook, "#!/bin/sh\necho SHOULD-NOT-RUN\n");
    f.write_ws_json(
        r#"{"repos":[{"name":"api","scripts":{"up":".drup-hooks/api-up.sh"}}]}"#,
    );

    let out = f.run(&["list"]);
    assert!(!out.status.success(), "the preflight must refuse a legacy anchor reference");
    let err = stderr(&out);
    assert!(
        err.contains("api.scripts.up") && err.contains(".drup-hooks/api-up.sh"),
        "the error must name the repo, event, and path:\n{err}"
    );
    assert!(
        err.contains("$DRUP_WORKSPACE_ROOT/.drup-hooks/api-up.sh"),
        "the error must show the exact migrated path:\n{err}"
    );
    assert!(
        err.contains("0.13"),
        "the error must name the release the change landed in so release notes find it:\n{err}"
    );
}

/// The preflight is file-existence-gated, so a bare `.drup-hooks/x.sh` with nothing at workspace level is not refused — that path is either a per-repo hook (new primary pattern) or a plain config bug the shell will report with full context.
#[test]
fn a_bare_hook_reference_with_no_workspace_file_is_not_touched_by_the_preflight() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(
        r#"{"repos":[{"name":"api","scripts":{"up":".drup-hooks/nothing-here.sh"}}]}"#,
    );
    f.run_ok(&["new", "api", "feat/no-file"]);
    let out = f.run(&["up", "feat/no-file"]);
    assert_eq!(out.status.code(), Some(1), "the shell still reports the miss");
    let err = stderr(&out);
    assert!(
        !err.contains("$DRUP_WORKSPACE_ROOT/.drup-hooks/nothing-here.sh"),
        "the preflight must not suggest a migration when there's no workspace file to migrate from:\n{err}"
    );
}

/// The new primary use case: a hook file committed inside the repo. A bare path now resolves from the worktree, so the file ships with the branch and tests + prod agree without a prefix.
#[test]
fn a_bare_hook_reference_resolves_from_the_worktree() {
    let f = WorkspaceFixture::empty();
    let api = f.add_repo("api");
    let hook_dir = api.join(".drup-hooks");
    std::fs::create_dir_all(&hook_dir).unwrap();
    write_exec(&hook_dir.join("up.sh"), "#!/bin/sh\necho HOOK-IN-REPO\n");
    f.git("api", &["add", ".drup-hooks/up.sh"]);
    f.git("api", &["commit", "-m", "add hook", "-q"]);
    f.write_ws_json(
        r#"{"repos":[{"name":"api","scripts":{"up":".drup-hooks/up.sh"}}]}"#,
    );
    f.run_ok(&["new", "api", "feat/in-repo"]);

    let out = f.run_ok(&["up", "feat/in-repo"]);
    let err = stderr(&out);
    assert!(
        err.contains("HOOK-IN-REPO"),
        "a .drup-hooks/ file committed in the repo should resolve from the worktree:\n{err}"
    );
}

/// The migration lane for a config that wants to keep a workspace-level hook. The prefix is explicit, the shell expands it, the file runs.
#[test]
fn an_explicit_workspace_root_prefix_resolves_a_workspace_level_file() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    let hook = f.path().join(".drup-hooks").join("api-up.sh");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    write_exec(&hook, "#!/bin/sh\necho HOOK-AT-WS\n");
    f.write_ws_json(
        r#"{"repos":[{"name":"api","scripts":{"up":"$DRUP_WORKSPACE_ROOT/.drup-hooks/api-up.sh"}}]}"#,
    );
    f.run_ok(&["new", "api", "feat/prefixed"]);

    let out = f.run_ok(&["up", "feat/prefixed"]);
    let err = stderr(&out);
    assert!(
        err.contains("HOOK-AT-WS"),
        "the explicit $DRUP_WORKSPACE_ROOT prefix must still reach workspace-level files:\n{err}"
    );
}
