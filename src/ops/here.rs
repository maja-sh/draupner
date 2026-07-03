use anyhow::{Context, Result};
use std::path::Path;

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

    let task = containing_task(&cwd, &worktrees_root).ok_or_else(|| {
        super::preflight("not inside a task worktree")
    })?;

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

/// If `cwd` is inside `<worktrees_root>/<task>/…`, return the task dir name.
/// Both paths are expected to be canonical.
fn containing_task(cwd: &Path, worktrees_root: &Path) -> Option<String> {
    let rel = cwd.strip_prefix(worktrees_root).ok()?;
    let first = rel.components().next()?;
    Some(first.as_os_str().to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn wt_root() -> PathBuf {
        PathBuf::from("/ws/proj/.worktrees")
    }

    #[test]
    fn detects_task_when_inside_repo_worktree() {
        let cwd = PathBuf::from("/ws/proj/.worktrees/feat-foo/api/src");
        assert_eq!(containing_task(&cwd, &wt_root()).as_deref(), Some("feat-foo"));
    }

    #[test]
    fn detects_task_at_the_task_dir_itself() {
        let cwd = PathBuf::from("/ws/proj/.worktrees/bug-bar");
        assert_eq!(containing_task(&cwd, &wt_root()).as_deref(), Some("bug-bar"));
    }

    #[test]
    fn none_at_worktrees_root_with_no_task_component() {
        assert_eq!(containing_task(&wt_root(), &wt_root()), None);
    }

    #[test]
    fn none_when_outside_worktrees_tree() {
        let cwd = PathBuf::from("/ws/proj/api/src");
        assert_eq!(containing_task(&cwd, &wt_root()), None);
    }

    #[test]
    fn none_for_sibling_prefix_collision() {
        // `.worktrees-backup` shares a textual prefix with `.worktrees` but is
        // not under it — component-wise strip_prefix must not be fooled.
        let cwd = PathBuf::from("/ws/proj/.worktrees-backup/feat-foo/api");
        assert_eq!(containing_task(&cwd, &wt_root()), None);
    }
}
