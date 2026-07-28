//! `ws exec [--branch <b>] [repo] [--local] [--no-flush] -- <command…>` — run a
//! command where the repo's stack lives. Remote workspace → `ssh -t` into the
//! synced worktree on the server; local (or `--local`) → run in the local
//! worktree. Same command either way; the child's stdio and exit code pass
//! straight through.
//!
//! The worktree is taken from `--branch` when given (so you can target any
//! worktree from anywhere, e.g. the workspace root), else inferred from the cwd.

use anyhow::{Context, Result};
use std::io::IsTerminal;
use std::path::Path;
use std::process::{Command, ExitStatus};

use crate::config;
use crate::inject;
use crate::mutagen;
use crate::remote;
use crate::workspace;

pub fn run(
    repo: Option<&str>,
    branch: Option<&str>,
    local: bool,
    no_flush: bool,
    command: &[String],
) -> Result<()> {
    // clap declares the command `required` after `--`, so it's always non-empty.
    let (program, args) = command.split_first().expect("clap enforces a non-empty command");

    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;

    // Which worktree: an explicit `--branch` targets any worktree from anywhere
    // (e.g. the workspace root); otherwise it — and the default repo — come from
    // the cwd, i.e. you're standing inside the worktree.
    let cwd = std::env::current_dir().context("resolve current directory")?;
    let cwd = cwd.canonicalize().unwrap_or(cwd);
    let root_canon = ws.root.canonicalize().unwrap_or_else(|_| ws.root.clone());
    let worktrees_root = root_canon.join(".worktrees");

    let (task, repo_from_cwd) = match branch {
        Some(b) => (workspace::sanitize_branch(b), None),
        None => workspace::task_repo_under(&cwd, &worktrees_root).ok_or_else(|| {
            super::preflight("not inside a worktree; pass `--branch <branch> <repo>`")
        })?,
    };

    let repo = match repo {
        Some(r) => r.to_string(),
        None => repo_from_cwd.ok_or_else(|| {
            super::preflight("can't infer the repo; pass it: `ws exec [--branch <b>] <repo> -- …`")
        })?,
    };
    if !cfg.repos.iter().any(|e| e.name() == repo) {
        return Err(super::preflight(format!("unknown repo `{repo}` (not in .ws.json)")));
    }
    // Fail here if the worktree doesn't exist — before any SSH. `ws new` always
    // creates the local `.worktrees/<task>/<repo>` (even for a remote workspace),
    // so a valid `--branch` always has a local dir; a bogus one gets a clean error
    // instead of an SSH round-trip to a `cd` into a nonexistent path.
    if workspace::worktree_for(&ws.root, &task, &repo).is_none() {
        return Err(super::preflight(format!("no worktree for `{repo}` in task `{task}`")));
    }

    let remote = if local { None } else { cfg.remote.as_ref() };

    // Give the command the same routing context as the scripts. A local run is
    // where the stack lives, so it allocates host ports (but doesn't render
    // templates — exec is a command runner); a remote run only *loads* the ports
    // recorded at `ws up` (Inspect). The injection site is `Local` either way:
    // the local branch must allocate locally, and the remote branch's Inspect
    // phase neither allocates nor derives nor writes the override, so it's
    // location-agnostic — this keeps `ws exec` from pre-dialing the server (its
    // deliberate no-pre-check behavior) yet still surfaces the recorded ports.
    let phase = if remote.is_none() { inject::Phase::Exec } else { inject::Phase::Inspect };
    let wt = workspace::task_dir(&ws.root, &task).join(&repo);
    let injected = inject::prepare(&cfg, &remote::Site::Local, &ws.root, &wt, &task, &repo, phase)?;
    let mut env: Vec<(&str, &str)> = vec![("WS_REPO", &repo), ("WS_BRANCH_SANITIZED", &task)];
    env.extend(injected.iter().map(|(k, v)| (k.as_str(), v.as_str())));

    let status = match remote {
        Some(r) => exec_remote(r, &ws.root, &task, &repo, no_flush, &env, command),
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
    c.args(args).current_dir(&wt);
    for (k, v) in env {
        c.env(k, v);
    }
    c.status().with_context(|| format!("run `{program}`"))
}

/// Run on the server via SSH, after flushing the worktree's sync so the far
/// side has our latest edits.
fn exec_remote(
    r: &config::RemoteConfig,
    ws_root: &Path,
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

    let dir = remote::worktree_path(&r.worktree_base(ws_root), task, repo);
    let script = remote::remote_script(&dir, env, command);
    // Allocate a remote pty only when both stdin and stdout are terminals.
    // Keying on stdin alone would put a pty on `ws exec … > file`, corrupting
    // the redirected output with CRLF / control sequences.
    let want_tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();

    crate::ui::note(&format!("→ {}:{dir}", r.connection));
    remote::ssh_command(&r.connection, &script, want_tty)
        .status()
        .with_context(|| format!("ssh {}", r.connection))
}

/// Mirror the child's exit status. Success → `Ok`. Non-zero or signal death →
/// a silent passthrough error carrying the code, so `ws` exits with it but
/// prints no banner over the command's own output.
fn propagate(status: ExitStatus) -> Result<()> {
    if status.success() {
        return Ok(());
    }
    Err(super::passthrough_exit(exit_code(&status)))
}

/// The child's exit code, mapping a signal death to `128 + signum` (so a
/// Ctrl-C'd child yields 130) to match `ws`'s exit conventions elsewhere.
#[cfg(unix)]
fn exit_code(status: &ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|s| 128 + s))
        .unwrap_or(1)
}

#[cfg(not(unix))]
fn exit_code(status: &ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}
