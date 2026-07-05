mod common;

use common::{stderr, stdout, WorkspaceFixture};
use std::path::Path;

/// Write a `.ws.json.local` with a remote block pointing at `connection`.
fn write_local_remote(f: &WorkspaceFixture, connection: &str) {
    std::fs::write(
        f.path().join(".ws.json.local"),
        format!(
            r#"{{"remote":{{"connection":"{connection}","workspace_path":"workspace"}}}}"#
        ),
    )
    .expect("write .ws.json.local");
}

/// Drop an executable stub script at `<root>/<name>` that appends each argument
/// (one per line) to the file named by env var `$ARGS_FILE`, then exits 0.
/// Stands in for `ssh`/`mutagen` so tests can assert what `ws` would invoke.
fn write_arg_recorder(f: &WorkspaceFixture, name: &str) -> std::path::PathBuf {
    let path = f.path().join(name);
    std::fs::write(
        &path,
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" >> \"$ARGS_FILE\"\n",
    )
    .expect("write recorder");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

#[test]
fn local_exec_runs_in_the_worktree() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/foo"]);

    // `touch marker` in the (inferred) worktree, then confirm it landed there.
    let out = f.run_in(".worktrees/feat-foo/api", &["exec", "--", "touch", "marker"]);
    assert!(out.status.success(), "exec should succeed:\n{}", stderr(&out));
    assert!(
        f.path().join(".worktrees/feat-foo/api/marker").exists(),
        "command should have run inside the api worktree"
    );
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
fn infers_repo_from_the_current_worktree() {
    let f = WorkspaceFixture::with_repos(&["api", "web"]);
    f.run_ok(&["new", "api,web", "feat/foo"]);

    // No repo arg — should target the worktree we're standing in (web).
    let out = f.run_in(".worktrees/feat-foo/web", &["exec", "--", "touch", "marker"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(f.path().join(".worktrees/feat-foo/web/marker").exists());
    assert!(!f.path().join(".worktrees/feat-foo/api/marker").exists());
}

#[test]
fn explicit_repo_targets_a_sibling() {
    let f = WorkspaceFixture::with_repos(&["api", "web"]);
    f.run_ok(&["new", "api,web", "feat/foo"]);

    // Standing in web, but ask for api explicitly.
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

    // From the workspace root — the task can't be inferred even with a repo arg.
    let out = f.run(&["exec", "api", "--", "true"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("not inside a task worktree"), "{}", stderr(&out));
}

#[test]
fn remote_workspace_dispatches_over_ssh() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/foo"]);
    write_local_remote(&f, "srv");
    let ssh = write_arg_recorder(&f, "fake-ssh");
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
    // No TTY in tests (stdin isn't a terminal), so no `-t`: just host + script.
    assert_eq!(lines[0], "srv", "first ssh arg is the connection");
    assert_eq!(
        lines[1],
        "cd workspace/feat-foo/api && WS_REPO=api WS_BRANCH_SANITIZED=feat-foo exec php artisan migrate",
        "remote script cds, exports WS_*, and execs the command"
    );
    // The command must NOT have run locally.
    assert!(!f.path().join(".worktrees/feat-foo/api/marker").exists());
}

#[test]
fn local_flag_forces_local_even_when_remote() {
    let f = WorkspaceFixture::with_repos(&["api"]);
    f.run_ok(&["new", "api", "feat/foo"]);
    write_local_remote(&f, "srv");
    let ssh = write_arg_recorder(&f, "fake-ssh");
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
