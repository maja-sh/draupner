use anyhow::{Context, Result};

use crate::config;
use crate::workspace;

pub fn run() -> Result<()> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;

    let cwd = std::env::current_dir().context("resolve current directory")?;
    let cwd = cwd.canonicalize().unwrap_or(cwd);

    // Canonicalize the workspace root so the containment test survives symlinks
    // anywhere in either path (e.g. macOS's /var → /private/var).
    let root_canon = ws.root.canonicalize().unwrap_or_else(|_| ws.root.clone());
    let worktrees_root = root_canon.join(".worktrees");

    let task = workspace::task_repo_under(&cwd, &worktrees_root)
        .map(|(task, _)| task)
        .ok_or_else(|| super::preflight("not inside a task worktree"))?;

    crate::ui::header("ws-here", &task);

    // Sibling worktrees for this task, in config order, keeping only the repos
    // that actually have a worktree. Display the non-canonical path so it reads
    // the same as `ws new`'s summary.
    let task_dir = ws.root.join(".worktrees").join(&task);
    let lines: Vec<String> = cfg
        .repos
        .iter()
        .map(|r| r.name())
        .filter_map(|repo| {
            let wt = task_dir.join(repo);
            wt.is_dir().then(|| format!("{repo}: {}", wt.display()))
        })
        .collect();

    crate::ui::summary_ok(&lines);
    Ok(())
}
