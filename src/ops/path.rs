use anyhow::Result;

use crate::config;
use crate::workspace;

pub fn run(branch: &str, repo: Option<&str>) -> Result<()> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;

    let sanitized = workspace::sanitize_branch(branch);
    if sanitized.is_empty() || sanitized.chars().all(|c| c == '.') {
        return Err(super::preflight(format!("invalid branch name: {branch}")));
    }

    let task_dir = workspace::task_dir(&ws.root, branch);
    if !task_dir.is_dir() {
        return Err(super::preflight(format!("no task worktree for `{branch}`")));
    }

    let path = match repo {
        None => task_dir,
        Some(r) => {
            let known: Vec<&str> = cfg.repos.iter().map(|e| e.name()).collect();
            if !known.contains(&r) {
                return Err(super::preflight(format!(
                    "unknown repo `{r}`. Known repos: {}",
                    known.join(", "),
                )));
            }
            workspace::worktree_for(&ws.root, branch, r).ok_or_else(|| {
                super::preflight(format!(
                    "task `{branch}` has no worktree for repo `{r}`",
                ))
            })?
        }
    };

    println!("{}", path.display());
    Ok(())
}
