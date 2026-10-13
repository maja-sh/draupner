use anyhow::Result;

use crate::config;
use crate::workspace;

pub fn run(branch: &str, repo: Option<&str>) -> Result<()> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;

    workspace::validate_branch(branch).map_err(super::preflight)?;

    let task_dir = workspace::task_dir(&ws.root, branch);
    if !task_dir.is_dir() {
        return Err(super::preflight(format!(
            "no worktrees for {branch}; run `wsx new` first"
        )));
    }

    let path = match repo {
        None => task_dir,
        Some(r) => {
            let known: Vec<&str> = cfg.repos.iter().map(|e| e.name()).collect();
            super::new::validate_repos(&[r.to_string()], &known)?;
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
