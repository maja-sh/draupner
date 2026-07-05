//! `ws exec [repo] [--local] [--no-flush] -- <command…>` — run a command where
//! the repo's stack lives. Remote workspace → `ssh -t` into the synced worktree
//! on the server; local (or `--local`) → run in the local worktree. Same
//! command either way; the child's stdio and exit code pass straight through.

use anyhow::{Context, Result};
use std::io::IsTerminal;
use std::path::Path;
use std::process::{Command, ExitStatus};

use crate::config;
use crate::mutagen;
use crate::remote;
use crate::workspace;

pub fn run(repo: Option<&str>, local: bool, no_flush: bool, command: &[String]) -> Result<()> {
    // clap declares the command `required` after `--`, so it's always non-empty.
    let (program, args) = command.split_first().expect("clap enforces a non-empty command");

    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;
    let local_cfg = config::load_local(&ws.root)?;

    // The task (which worktree set) always comes from CWD — you run `ws exec`
    // from inside a task's tree. The repo defaults to the one you're in, but an
    // explicit arg lets you target a sibling.
    let cwd = std::env::current_dir().context("resolve current directory")?;
    let cwd = cwd.canonicalize().unwrap_or(cwd);
    let root_canon = ws.root.canonicalize().unwrap_or_else(|_| ws.root.clone());
    let worktrees_root = root_canon.join(".worktrees");

    let (task, repo_from_cwd) = workspace::task_repo_under(&cwd, &worktrees_root)
        .ok_or_else(|| super::preflight("not inside a task worktree; run `ws exec` from within one"))?;

    let repo = match repo {
        Some(r) => r.to_string(),
        None => repo_from_cwd.ok_or_else(|| {
            super::preflight("can't infer the repo from here; pass it: `ws exec <repo> -- …`")
        })?,
    };
    if !cfg.repos.iter().any(|e| e.name() == repo) {
        return Err(super::preflight(format!("unknown repo `{repo}` (not in .ws.json)")));
    }

    let remote = if local { None } else { local_cfg.remote.as_ref() };
    let env: [(&str, &str); 2] = [("WS_REPO", &repo), ("WS_BRANCH_SANITIZED", &task)];

    let status = match remote {
        Some(r) => exec_remote(r, &task, &repo, no_flush, &env, command),
        None => exec_local(&ws.root, &task, &repo, &env, program, args),
    }?;

    propagate(status)
}

/// Run in the local worktree at `<root>/.worktrees/<task>/<repo>`.
fn exec_local(
    root: &Path,
    task: &str,
    repo: &str,
    env: &[(&str, &str)],
    program: &str,
    args: &[String],
) -> Result<ExitStatus> {
    let wt = workspace::worktree_for(root, task, repo).ok_or_else(|| {
        super::preflight(format!("no worktree for `{repo}` in task `{task}`"))
    })?;
    let mut c = Command::new(program);
    c.args(args).current_dir(&wt).env("WS_WORKSPACE_ROOT", root);
    for (k, v) in env {
        c.env(k, v);
    }
    c.status().with_context(|| format!("run `{program}`"))
}

/// Run on the server via SSH, after flushing the worktree's sync so the far
/// side has our latest edits.
fn exec_remote(
    r: &config::RemoteConfig,
    task: &str,
    repo: &str,
    no_flush: bool,
    env: &[(&str, &str)],
    command: &[String],
) -> Result<ExitStatus> {
    if !no_flush {
        let session = mutagen::session_name(task, repo);
        // Best-effort: the session may not exist yet (sync lifecycle isn't wired
        // into `up` yet) or mutagen may be absent. Note it and carry on.
        if let Err(e) = mutagen::flush(&session) {
            crate::ui::skip("sync", &format!("flush skipped ({e})"));
        }
    }

    let dir = remote::worktree_path(&r.workspace_path, task, repo);
    let script = remote::remote_script(&dir, env, command);
    let want_tty = std::io::stdin().is_terminal();

    crate::ui::note(&format!("→ {}:{dir}", r.connection));
    remote::ssh_command(&r.connection, &script, want_tty)
        .status()
        .with_context(|| format!("ssh {}", r.connection))
}

/// Mirror the child's exit status. Success → `Ok`. Non-zero (or signal) →
/// a silent passthrough error carrying the code, so `ws` exits with it but
/// prints no banner over the command's own output.
fn propagate(status: ExitStatus) -> Result<()> {
    if status.success() {
        return Ok(());
    }
    Err(super::passthrough_exit(status.code().unwrap_or(1)))
}
