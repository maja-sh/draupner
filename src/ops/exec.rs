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
    // clap enforces a non-empty command (`last`/`required`), but stay defensive.
    let Some((program, args)) = command.split_first() else {
        return Err(super::preflight("no command given; use `ws exec [repo] -- <command>`"));
    };

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

    let (task, repo_from_cwd) = containing_task_repo(&cwd, &worktrees_root)
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
    if !remote::local_worktree_exists(root, task, repo) {
        return Err(super::preflight(format!(
            "no worktree for `{repo}` in task `{task}`"
        )));
    }
    let wt = root.join(".worktrees").join(task).join(repo);
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

/// If `cwd` is inside `<worktrees_root>/<task>[/<repo>/…]`, return the task dir
/// name and (when the path reaches into a repo) that repo name. Both paths are
/// expected to be canonical. Mirrors `here::containing_task`, extended to also
/// pull out the repo component.
fn containing_task_repo(cwd: &Path, worktrees_root: &Path) -> Option<(String, Option<String>)> {
    let rel = cwd.strip_prefix(worktrees_root).ok()?;
    let mut comps = rel.components();
    let task = comps.next()?.as_os_str().to_string_lossy().into_owned();
    let repo = comps.next().map(|c| c.as_os_str().to_string_lossy().into_owned());
    Some((task, repo))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn wt_root() -> PathBuf {
        PathBuf::from("/ws/proj/.worktrees")
    }

    #[test]
    fn infers_task_and_repo_from_inside_a_repo_worktree() {
        let cwd = PathBuf::from("/ws/proj/.worktrees/feat-foo/api/src");
        assert_eq!(
            containing_task_repo(&cwd, &wt_root()),
            Some(("feat-foo".to_string(), Some("api".to_string())))
        );
    }

    #[test]
    fn infers_task_but_no_repo_at_the_task_dir() {
        let cwd = PathBuf::from("/ws/proj/.worktrees/bug-bar");
        assert_eq!(
            containing_task_repo(&cwd, &wt_root()),
            Some(("bug-bar".to_string(), None))
        );
    }

    #[test]
    fn none_outside_the_worktrees_tree() {
        let cwd = PathBuf::from("/ws/proj/api/src");
        assert_eq!(containing_task_repo(&cwd, &wt_root()), None);
    }

    #[test]
    fn none_for_sibling_prefix_collision() {
        let cwd = PathBuf::from("/ws/proj/.worktrees-backup/feat-foo/api");
        assert_eq!(containing_task_repo(&cwd, &wt_root()), None);
    }
}
