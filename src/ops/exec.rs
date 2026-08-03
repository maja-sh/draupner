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

    let cwd = std::env::current_dir().context("resolve current directory")?;
    let cwd = cwd.canonicalize().unwrap_or(cwd);
    let worktrees_root = workspace::worktrees_root(&ws.root);

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
    // Checked before any SSH: `ws new` creates the local `.worktrees/<task>/<repo>`
    // even for a remote workspace, so a bogus `--branch` gets a clean error here
    // rather than an SSH round-trip that `cd`s into a nonexistent path.
    let wt = workspace::worktree_for(&ws.root, &task, &repo)
        .ok_or_else(|| super::preflight(format!("no worktree for `{repo}` in task `{task}`")))?;

    let remote = if local { None } else { cfg.remote.as_ref() };

    // The injection site is `Local` even for a remote run, which looks wrong: the
    // `Inspect` phase only reads the ports recorded at `ws up` — it allocates
    // nothing, derives nothing, writes no override — so it is location-agnostic,
    // and resolving a remote site here would make `ws exec` dial the server, which
    // it deliberately never does before running your command.
    let phase = if remote.is_none() { inject::Phase::Exec } else { inject::Phase::Inspect };
    let injected = inject::prepare(&cfg, &remote::Site::Local, &ws.root, &wt, &task, &repo, phase)?;
    let mut env: Vec<(&str, &str)> = vec![("WS_REPO", &repo), ("WS_BRANCH_SANITIZED", &task)];
    env.extend(injected.iter().map(|(k, v)| (k.as_str(), v.as_str())));

    let status = match remote {
        Some(r) => exec_remote(r, &ws.root, &task, &repo, no_flush, &env, command),
        None => exec_local(&wt, &env, program, args),
    }?;

    propagate(status)
}

fn exec_local(
    wt: &Path,
    env: &[(&str, &str)],
    program: &str,
    args: &[String],
) -> Result<ExitStatus> {
    let mut c = Command::new(program);
    c.args(args).current_dir(wt);
    for (k, v) in env {
        c.env(k, v);
    }
    c.status().with_context(|| format!("run `{program}`"))
}

/// Flushes the worktree's sync first, so the server runs against our latest edits.
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
        // Best-effort: the session may not exist yet, or mutagen may be absent.
        if let Err(e) = mutagen::flush(&session) {
            crate::ui::skip("sync", &format!("flush skipped ({e})"));
        }
    }

    // Deliberately NOT `Site::worktree_at`: that home-resolves the base to an
    // absolute path, which needs a resolved `Site` — i.e. dialing the server for
    // its `$HOME`, the pre-check `ws exec` avoids. A relative base is fine here
    // because the `cd` is the first thing in the script and nothing afterwards
    // resolves a path against a different cwd.
    let dir = remote::worktree_path(&r.worktree_base(ws_root), task, repo);
    let script = remote::remote_script(&dir, env, command);
    // Both streams, not just stdin: keying on stdin alone would put a pty on
    // `ws exec … > file` and corrupt the redirect with CRLF / control sequences.
    let want_tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();

    crate::ui::note(&format!("→ {}:{dir}", r.connection));
    remote::ssh_command(&r.connection, &script, want_tty)
        .status()
        .with_context(|| format!("ssh {}", r.connection))
}

/// A failing child becomes a silent passthrough error, so `ws` exits with its
/// code but prints no banner over the command's own output.
fn propagate(status: ExitStatus) -> Result<()> {
    if status.success() {
        return Ok(());
    }
    Err(super::passthrough_exit(exit_code(&status)))
}

/// Signal death maps to `128 + signum` (a Ctrl-C'd child yields 130), matching
/// `ws`'s exit conventions elsewhere.
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
