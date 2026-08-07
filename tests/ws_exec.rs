mod common;

use common::{stderr, stdout, WorkspaceFixture};
use std::path::Path;

fn write_remote(f: &WorkspaceFixture, connection: &str) {
    std::fs::write(
        f.path().join(".ws.json"),
        format!(
            r#"{{"repos":["api"],"remote":{{"connection":"{connection}","workspace_path":"workspace"}}}}"#
        ),
    )
    .expect("write .ws.json");
}

#[test]
fn local_exec_runs_in_the_worktree() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/foo"]);

    let out = f.run_in(".worktrees/feat-foo/api", &["exec", "--", "touch", "marker"]);
    assert!(out.status.success(), "exec should succeed:\n{}", stderr(&out));
    assert!(
        f.path().join(".worktrees/feat-foo/api/marker").exists(),
        "command should have run inside the api worktree"
    );
}

#[test]
fn branch_flag_targets_a_worktree_from_the_workspace_root() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/foo"]);

    let out = f.run_ok(&["exec", "--branch", "feat/foo", "api", "--", "touch", "marker"]);
    assert!(out.status.success(), "exec --branch should succeed:\n{}", stderr(&out));
    assert!(
        f.path().join(".worktrees/feat-foo/api/marker").exists(),
        "command should have run inside the api worktree, targeted by --branch"
    );
}

#[test]
fn branch_flag_without_a_worktree_is_a_clean_error() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    let out = f.run(&["exec", "--branch", "nope", "api", "--", "true"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("no worktree"), "expected a no-worktree error:\n{}", stderr(&out));
}

#[test]
fn exposes_ws_env_to_the_command() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/foo"]);

    let out = f.run_in(
        ".worktrees/feat-foo/api",
        &["exec", "api", "--", "sh", "-c", "echo $WS_REPO-$WS_BRANCH_SANITIZED"],
    );
    assert!(out.status.success());
    assert!(stdout(&out).contains("api-feat-foo"), "stdout was:\n{}", stdout(&out));
}

#[test]
fn propagates_the_child_exit_code_without_a_banner() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/foo"]);

    let out = f.run_in(".worktrees/feat-foo/api", &["exec", "--", "sh", "-c", "exit 3"]);
    assert_eq!(out.status.code(), Some(3), "should mirror the child's exit code");
    assert!(
        !stderr(&out).contains("FAIL"),
        "a transparent runner must not print its own failure banner:\n{}",
        stderr(&out)
    );
}

#[test]
fn propagates_signal_death_as_128_plus_signum() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/foo"]);

    let out = f.run_in(".worktrees/feat-foo/api", &["exec", "--", "sh", "-c", "kill -TERM $$"]);
    assert_eq!(out.status.code(), Some(143), "expected 128+SIGTERM");
    assert!(!stderr(&out).contains("FAIL"), "no banner over a signalled child:\n{}", stderr(&out));
}

#[test]
fn infers_repo_from_the_current_worktree() {
    let f = WorkspaceFixture::with_repos(&["api", "web"]);
    f.run_ok(&["new", "api,web", "feat/foo"]);

    let out = f.run_in(".worktrees/feat-foo/web", &["exec", "--", "touch", "marker"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(f.path().join(".worktrees/feat-foo/web/marker").exists());
    assert!(!f.path().join(".worktrees/feat-foo/api/marker").exists());
}

#[test]
fn explicit_repo_targets_a_sibling() {
    let f = WorkspaceFixture::with_repos(&["api", "web"]);
    f.run_ok(&["new", "api,web", "feat/foo"]);

    let out = f.run_in(".worktrees/feat-foo/web", &["exec", "api", "--", "touch", "marker"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(f.path().join(".worktrees/feat-foo/api/marker").exists());
    assert!(!f.path().join(".worktrees/feat-foo/web/marker").exists());
}

#[test]
fn unknown_repo_is_a_preflight_error() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/foo"]);

    let out = f.run_in(".worktrees/feat-foo/api", &["exec", "nope", "--", "true"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("unknown repo"), "{}", stderr(&out));
}

#[test]
fn outside_a_worktree_is_a_preflight_error() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/foo"]);

    let out = f.run(&["exec", "api", "--", "true"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("not inside a worktree"), "{}", stderr(&out));
    assert!(stderr(&out).contains("--branch"), "should suggest --branch:\n{}", stderr(&out));
}

#[test]
fn remote_workspace_dispatches_over_ssh() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/foo"]);
    write_remote(&f, "srv");
    let ssh = f.arg_recorder("fake-ssh");
    let args_file = f.path().join("ssh-args.txt");

    let out = f.run_in_env(
        ".worktrees/feat-foo/api",
        &["exec", "api", "--no-flush", "--", "php", "artisan", "migrate"],
        &[
            ("WS_SSH", ssh.to_str().unwrap()),
            ("ARGS_FILE", args_file.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));

    let recorded = std::fs::read_to_string(&args_file).expect("ssh was invoked");
    let lines: Vec<&str> = recorded.lines().collect();
    // ControlMaster options prefix the args, so the connection and script are the final two.
    assert_eq!(lines[lines.len() - 2], "srv", "the connection precedes the script");
    assert_eq!(
        lines[lines.len() - 1],
        "cd workspace/feat-foo/api && WS_REPO=api WS_BRANCH_SANITIZED=feat-foo \
         WS_RUNTIME=podman WS_REMOTE=1 WS_CONNECTION=srv exec php artisan migrate",
        "remote script cds, exports WS_* (incl. runtime + remoteness), and execs the command"
    );
    assert!(!f.path().join(".worktrees/feat-foo/api/marker").exists());
}

#[test]
fn local_flag_forces_local_even_when_remote() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/foo"]);
    write_remote(&f, "srv");
    let ssh = f.arg_recorder("fake-ssh");
    let args_file = f.path().join("ssh-args.txt");

    let out = f.run_in_env(
        ".worktrees/feat-foo/api",
        &["exec", "--local", "--", "touch", "marker"],
        &[
            ("WS_SSH", ssh.to_str().unwrap()),
            ("ARGS_FILE", args_file.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(f.path().join(".worktrees/feat-foo/api/marker").exists(), "should run locally");
    assert!(!Path::new(&args_file).exists(), "ssh must not be invoked with --local");
}

#[test]
fn missing_command_is_a_usage_error() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/foo"]);

    let out = f.run_in(".worktrees/feat-foo/api", &["exec", "api"]);
    assert_eq!(out.status.code(), Some(2), "clap usage error exits 2");
}
