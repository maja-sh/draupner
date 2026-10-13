//! `wsx exec` — run a command in a repo's worktree, remote via `ssh -t` or local, with stdio and exit code passing straight through.

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
            super::preflight("can't infer the repo; pass it: `wsx exec [--branch <b>] <repo> -- …`")
        })?,
    };
    if !cfg.repos.iter().any(|e| e.name() == repo) {
        return Err(super::preflight(format!("unknown repo `{repo}` (not in .ws.json)")));
    }
    // Check locally before any SSH: `wsx new` always creates the local dir, so a bogus `--branch` fails clean instead of round-tripping to `cd` into a missing path.
    if workspace::worktree_for(&ws.root, &task, &repo).is_none() {
        return Err(super::preflight(format!("no worktree for `{repo}` in task `{task}`")));
    }

    let remote = if local { None } else { cfg.remote.as_ref() };

    // Local run allocates host ports (Exec); remote only loads ports recorded at `wsx up` (Inspect). Passing `Site::Local` unconditionally keeps `wsx exec` from pre-dialing the server.
    let phase = if remote.is_none() { inject::Phase::Exec } else { inject::Phase::Inspect };
    let wt = workspace::task_dir(&ws.root, &task).join(&repo);
    let injected = inject::prepare(&cfg, &remote::Site::Local, &ws.root, &wt, &task, &repo, phase)?;

    let status = match remote {
        Some(r) => exec_remote(r, &ws.root, &task, &repo, no_flush, &injected, command),
        None => exec_local(&ws.root, &wt, &task, &repo, &injected, program, args),
    }?;

    propagate(status)
}

/// Base `WS_*` env every dispatched command sees. Deliberately does NOT set
/// `WS_HOOK_NAME` — `wsx exec` isn't a hook, so lying about the event context
/// would surprise a script that special-cases lifecycle events.
///
/// Kept in lockstep with `hooks::base_env`; the parity test in this module
/// asserts they agree on every key except `WS_HOOK_NAME`.
fn build_env<'a>(
    workspace_root: &'a str,
    worktree: &'a str,
    repo: &'a str,
    branch_sanitized: &'a str,
    injected: &'a [(String, String)],
) -> Vec<(&'a str, &'a str)> {
    let mut env = vec![
        ("WS_WORKSPACE_ROOT", workspace_root),
        ("WS_WORKTREE", worktree),
        ("WS_REPO", repo),
        ("WS_BRANCH_SANITIZED", branch_sanitized),
    ];
    env.extend(injected.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    env
}

/// Run in the local worktree at `<root>/.worktrees/<task>/<repo>`.
fn exec_local(
    root: &Path,
    wt: &Path,
    task: &str,
    repo: &str,
    injected: &[(String, String)],
    program: &str,
    args: &[String],
) -> Result<ExitStatus> {
    let root_s = root.to_string_lossy();
    let wt_s = wt.to_string_lossy();
    let env = build_env(&root_s, &wt_s, repo, task, injected);

    let mut c = Command::new(program);
    c.args(args).current_dir(wt);
    for (k, v) in &env {
        c.env(k, v);
    }
    c.status().with_context(|| format!("run `{program}`"))
}

/// Run on the server via SSH, flushing the worktree's sync first so the far side has our latest edits.
fn exec_remote(
    r: &config::RemoteConfig,
    ws_root: &Path,
    task: &str,
    repo: &str,
    no_flush: bool,
    injected: &[(String, String)],
    command: &[String],
) -> Result<ExitStatus> {
    if !no_flush {
        let session = mutagen::session_name(task, repo);
        // Best-effort: the session may not exist yet, or mutagen may be absent.
        if let Err(e) = mutagen::flush(&session) {
            crate::ui::skip("sync", &format!("flush skipped ({e})"));
        }
    }

    let base = r.worktree_base(ws_root);
    let dir = remote::worktree_path(&base, task, repo);
    let mut env = build_env(&base, &dir, repo, task, injected);
    // Version-manager shims (nvm, rbenv, pyenv) install into `.zshrc`/`.bashrc`
    // and aren't on non-interactive SSH's PATH; harvest it once per connection.
    // Appended last so a repo's declared PATH (via `injected`) still wins.
    let harvested = remote::login_path(&r.connection);
    if let Some(p) = &harvested {
        if !injected.iter().any(|(k, _)| k == "PATH") {
            env.push(("PATH", p));
        }
    }

    let script = remote::remote_script(&dir, &env, command);
    // Both TTYs required: keying on stdin alone puts a pty on `wsx exec … > file` and corrupts the redirect with CRLF/control sequences.
    let want_tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();

    crate::ui::note(&format!("→ {}:{dir}", r.connection));
    remote::ssh_command(&r.connection, &script, want_tty)
        .status()
        .with_context(|| format!("ssh {}", r.connection))
}

/// Mirror the child's exit status; non-zero or signal death becomes a silent passthrough error so `wsx` exits with the code but prints no banner over the command's own output.
fn propagate(status: ExitStatus) -> Result<()> {
    if status.success() {
        return Ok(());
    }
    Err(super::passthrough_exit(exit_code(&status)))
}

/// Child's exit code, mapping signal death to `128 + signum` (Ctrl-C → 130) to match `wsx`'s exit conventions elsewhere.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The parity invariant: every `WS_*` key `hooks::base_env` sets — except
    /// `WS_HOOK_NAME`, which exec deliberately omits — must appear in
    /// `build_env` with the same value. Prevents `wsx exec` from silently
    /// drifting away from the hook env contract when a new key is added.
    #[test]
    fn build_env_matches_hooks_base_env_minus_hook_name() {
        let root = Path::new("/ws");
        let wt = Path::new("/ws/.worktrees/foo/api");
        let root_s = root.to_string_lossy();
        let wt_s = wt.to_string_lossy();
        let injected: Vec<(String, String)> = vec![
            ("WS_PORT_mysql".to_string(), "20001".to_string()),
            ("WS_RUNTIME".to_string(), "podman".to_string()),
        ];

        let env = build_env(&root_s, &wt_s, "api", "foo", &injected);
        let hook_env = crate::hooks::base_env(root, &wt_s, "api", "foo", "up");

        for (k, v) in &hook_env {
            if k == "WS_HOOK_NAME" {
                continue;
            }
            let got = env.iter().find(|(ek, _)| *ek == k.as_str()).map(|(_, ev)| *ev);
            assert_eq!(got, Some(v.as_str()), "exec must match hook for {k}");
        }
        assert!(
            !env.iter().any(|(k, _)| *k == "WS_HOOK_NAME"),
            "exec must not set WS_HOOK_NAME — it isn't a hook",
        );
        assert_eq!(
            env.iter().find(|(k, _)| *k == "WS_PORT_mysql").map(|(_, v)| *v),
            Some("20001"),
            "injected env must survive",
        );
    }
}
