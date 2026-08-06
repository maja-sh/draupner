//! Declarative `scripts` in `.ws.json` — the successor to `.ws-hooks/` files.

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

#[test]
fn script_takes_precedence_over_hook_file() {
    let f = fixture_with_scripts(r#"{"up":"echo FROM-SCRIPT"}"#);
    f.add_hook("api", "up", "echo FROM-HOOK-FILE");
    f.run_ok(&["new", "api", "feat/prec"]);

    let out = f.run_ok(&["up", "feat/prec"]);
    let err = stderr(&out);
    assert!(err.contains("FROM-SCRIPT"), "script should win:\n{err}");
    assert!(
        !err.contains("FROM-HOOK-FILE"),
        "the hook file must not run when a script is declared:\n{err}"
    );
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
