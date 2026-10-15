//! Declarative `scripts` in `.drup.json` — the only lifecycle mechanism; also covers the migration error for a leftover executable `.drup-hooks/<repo>/<event>` file.

mod common;

use common::{stderr, WorkspaceFixture};

fn fixture_with_scripts(scripts_json: &str) -> WorkspaceFixture {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    f.write_ws_json(&format!(
        r#"{{"repos":[{{"name":"api","scripts":{scripts_json}}}]}}"#
    ));
    f
}

#[test]
fn declarative_up_script_runs() {
    let f = fixture_with_scripts(r#"{"up":"echo SCRIPT-UP-RAN"}"#);
    f.run_ok(&["new", "api", "feat/foo"]);

    let out = f.run_ok(&["up", "feat/foo"]);
    let err = stderr(&out);
    assert!(err.contains("SCRIPT-UP-RAN"), "expected script output in:\n{err}");
    assert_eq!(err.matches("up hook").count(), 1, "expected one up step in:\n{err}");
}

#[test]
fn declarative_up_script_multi_command_runs_in_order() {
    let f = fixture_with_scripts(r#"{"up":["echo FIRST","echo SECOND"]}"#);
    f.run_ok(&["new", "api", "feat/multi"]);

    let out = f.run_ok(&["up", "feat/multi"]);
    let err = stderr(&out);
    let first = err.find("FIRST").expect("FIRST in output");
    let second = err.find("SECOND").expect("SECOND in output");
    assert!(first < second, "commands should run in order:\n{err}");
}

/// Write an executable `.drup-hooks/<repo>/<event>` — the layout the removed hook-file mechanism discovered by convention.
fn write_legacy_hook_file(f: &WorkspaceFixture, repo: &str, event: &str) {
    let dir = f.path().join(".drup-hooks").join(repo);
    std::fs::create_dir_all(&dir).expect("create hook dir");
    let path = dir.join(event);
    std::fs::write(&path, "#!/usr/bin/env bash\necho FROM-HOOK-FILE\n").expect("write hook file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}

// Hook files discovered by convention are gone. Dropping one silently would turn
// a working `up` into the compose default (or nothing), so the repo fails with
// the `scripts` entry to paste and the two contract changes that come with it.
#[test]
fn a_leftover_hook_file_fails_the_repo_with_the_migration() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    write_legacy_hook_file(&f, "api", "up");
    f.run_ok(&["new", "api", "feat/legacy"]);

    let out = f.run(&["up", "feat/legacy"]);
    assert_eq!(out.status.code(), Some(1), "a leftover hook file must fail the repo");
    let err = stderr(&out);
    assert!(!err.contains("FROM-HOOK-FILE"), "the hook file must not run:\n{err}");
    assert!(
        err.contains(r#""scripts": {"up": ".drup-hooks/api/up"}"#),
        "the error must name the exact JSON to paste:\n{err}"
    );
    assert!(err.contains("$DRUP_WORKTREE"), "the error must name the new worktree var:\n{err}");
}

// ...and declaring it is the whole migration: the same file, now named under
// `scripts`, runs again (with the new contract).
#[test]
fn declaring_a_hook_file_under_scripts_runs_it_again() {
    let f = WorkspaceFixture::empty();
    f.add_repo("api");
    write_legacy_hook_file(&f, "api", "up");
    f.write_ws_json(
        r#"{"repos":[{"name":"api","scripts":{"up":".drup-hooks/api/up"}}]}"#,
    );
    f.run_ok(&["new", "api", "feat/migrated"]);

    let err = stderr(&f.run_ok(&["up", "feat/migrated"]));
    assert!(err.contains("FROM-HOOK-FILE"), "the declared script should run:\n{err}");
}

#[test]
fn declarative_up_script_failure_exits_nonzero() {
    let f = fixture_with_scripts(r#"{"up":"echo TRIED; exit 1"}"#);
    f.run_ok(&["new", "api", "feat/fail"]);

    let out = f.run(&["up", "feat/fail"]);
    assert_eq!(out.status.code(), Some(1), "a failing script should exit 1");
    let err = stderr(&out);
    assert!(err.contains("TRIED"), "the script should have been attempted:\n{err}");
}

#[test]
fn multi_command_script_stops_at_first_failure() {
    let f = fixture_with_scripts(r#"{"up":["echo ONE","exit 1","echo THREE"]}"#);
    f.run_ok(&["new", "api", "feat/faststop"]);

    let out = f.run(&["up", "feat/faststop"]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("ONE"), "first command should run:\n{err}");
    assert!(!err.contains("THREE"), "commands after a failure must not run:\n{err}");
}

#[test]
fn multi_command_script_shares_one_shell() {
    // A `Many` runs in a single shell so state carries; a process-per-command would print `val=`.
    let f = fixture_with_scripts(r#"{"up":["export FOO=bar","echo val=$FOO"]}"#);
    f.run_ok(&["new", "api", "feat/shell"]);

    let out = f.run_ok(&["up", "feat/shell"]);
    let err = stderr(&out);
    assert!(err.contains("val=bar"), "state should carry across commands in one shell:\n{err}");
}

#[test]
fn declarative_post_create_script_runs_on_new() {
    let f = fixture_with_scripts(r#"{"post-create":"echo POST-CREATE-RAN"}"#);

    let out = f.run_ok(&["new", "api", "feat/pc"]);
    let err = stderr(&out);
    assert!(err.contains("POST-CREATE-RAN"), "expected post-create output in:\n{err}");
}

#[test]
fn script_runs_in_the_worktree_dir() {
    let f = fixture_with_scripts(r#"{"up":"pwd"}"#);
    f.run_ok(&["new", "api", "feat/cwd"]);

    let out = f.run_ok(&["up", "feat/cwd"]);
    let err = stderr(&out);
    assert!(
        err.contains(".worktrees/feat-cwd/api"),
        "script cwd should be the worktree:\n{err}"
    );
}

#[test]
fn a_legacy_post_create_file_is_refused_before_any_worktree_is_created() {
    let f = WorkspaceFixture::with_repos(&["api", "app"]);
    // The refusal used to reach `drup new` through the per-repo creation loop, so
    // it landed after `git worktree add` and the sync session for `api` — and the
    // migration it asks for then couldn't be retried, because the half-created
    // task blocked `drup new` for the same branch.
    write_legacy_hook_file(&f, "app", "post-create");

    let out = f.run(&["new", "api,app", "feat/legacy-pc"]);
    assert_eq!(out.status.code(), Some(2), "expected a preflight refusal: {}", stderr(&out));
    assert!(stderr(&out).contains("drup 0.6"), "must be the migration message: {}", stderr(&out));

    // Nothing may exist yet — not even for `api`, which sorts before the repo
    // that carries the stale file.
    let task = f.path().join(".worktrees").join("feat-legacy-pc");
    assert!(!task.join("api").exists(), "no worktree may survive a preflight refusal");
    assert!(!task.join("app").exists(), "no worktree may survive a preflight refusal");

    // And declaring the script suppresses it — the preflight must mirror
    // `resolve`'s condition, not refuse a repo that has already migrated while
    // still owning the old file.
    f.add_hook("app", "post-create", "echo MIGRATED");
    f.run_ok(&["new", "api,app", "feat/legacy-pc2"]);
}
