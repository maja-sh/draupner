use anyhow::{bail, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub struct Workspace {
    pub root: PathBuf,
}

/// Walk upward from CWD for the nearest `.ws.json`.
pub fn locate() -> Result<Workspace> {
    let start = std::env::current_dir()?;
    let mut cur = start.as_path();
    loop {
        if cur.join(".ws.json").is_file() {
            return Ok(Workspace { root: cur.to_path_buf() });
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => bail!("no `.ws.json` found from {} upward", start.display()),
        }
    }
}

/// Sanitize a branch name for filesystem use — replaces `/` with `-`.
pub fn sanitize_branch(b: &str) -> String {
    b.replace('/', "-")
}

/// `<root>/.worktrees/<sanitized-branch>/`
pub fn task_dir(root: &Path, branch: &str) -> PathBuf {
    root.join(".worktrees").join(sanitize_branch(branch))
}

/// `<root>/.worktrees/<sanitized-branch>/<repo>/` if the worktree exists on disk.
pub fn worktree_for(root: &Path, branch: &str, repo: &str) -> Option<PathBuf> {
    let p = task_dir(root, branch).join(repo);
    p.is_dir().then_some(p)
}

/// Filesystem view of a branch's repo dirs, kept because a directory git doesn't know may still hold teardown resources.
pub fn all_worktree_dirs_for(root: &Path, branch: &str) -> Vec<(String, PathBuf)> {
    list_child_dirs(&task_dir(root, branch))
        .into_iter()
        .filter_map(|(name, path)| name.map(|n| (n, path)))
        .collect()
}

/// Filesystem view of task dirs under `.worktrees/`, kept so [`orphan_dirs`] can compare against git.
pub fn all_task_dirs(root: &Path) -> Vec<PathBuf> {
    list_child_dirs(&root.join(".worktrees"))
        .into_iter()
        .map(|(_, path)| path)
        .collect()
}

/// One task worktree, as **git** knows it.
pub struct Worktree {
    pub repo: String,
    pub path: PathBuf,
    /// The checked-out branch; `None` for a detached HEAD.
    pub branch: Option<String>,
    /// The `.worktrees/<task>` dir when ws-managed; `None` for hand-made worktrees elsewhere.
    pub task: Option<String>,
}

/// Every task worktree across configured repos, sourced from git (not the filesystem) so
/// stale registrations, out-of-tree worktrees, and sanitized-name collisions are handled
/// correctly; the main checkout is excluded.
pub fn worktrees(root: &Path, cfg: &crate::config::WsConfig) -> Vec<Worktree> {
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let worktrees_root = canonical_root.join(".worktrees");

    let mut out = Vec::new();
    for entry in &cfg.repos {
        let repo = entry.name();
        let repo_path = root.join(repo);
        if !is_git_repo(&repo_path) {
            continue;
        }
        let repo_canon = repo_path.canonicalize().unwrap_or_else(|_| repo_path.clone());

        for (path, branch) in crate::git::worktree_list(&repo_path) {
            let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
            if canon == repo_canon {
                continue; // the main checkout, not a task worktree
            }
            // The repo component is implied by which repo reported this, so only the task is read back.
            let task = task_repo_under(&canon, &worktrees_root).map(|(task, _)| task);
            out.push(Worktree { repo: repo.to_string(), path, branch, task });
        }
    }
    out
}

/// Branch-scoped [`worktrees`]; matches on task-dir name as a fallback so detached-HEAD worktrees still resolve.
pub fn worktrees_for(
    root: &Path,
    cfg: &crate::config::WsConfig,
    branch: &str,
) -> Vec<(String, PathBuf)> {
    let task = sanitize_branch(branch);
    worktrees(root, cfg)
        .into_iter()
        .filter(|w| w.branch.as_deref() == Some(branch) || w.task.as_deref() == Some(task.as_str()))
        .map(|w| (w.repo, w.path))
        .collect()
}

/// Directories under `.worktrees/` git doesn't know — reported by `ws clean`, never acted
/// on, because silently syncing a leftover to a server as if it were real is the failure mode.
pub fn orphan_dirs(root: &Path, known: &[Worktree]) -> Vec<PathBuf> {
    let known: Vec<PathBuf> = known
        .iter()
        .map(|w| w.path.canonicalize().unwrap_or_else(|_| w.path.clone()))
        .collect();

    let mut out = Vec::new();
    for task_dir in all_task_dirs(root) {
        for (_, repo_dir) in list_child_dirs(&task_dir) {
            let canon = repo_dir.canonicalize().unwrap_or_else(|_| repo_dir.clone());
            if !known.contains(&canon) {
                out.push(repo_dir);
            }
        }
    }
    out.sort();
    out
}

/// Given a canonical `path` inside `<worktrees_root>/<task>[/<repo>/…]`, return the task
/// and optional repo — the shared CWD→task/repo inference.
pub fn task_repo_under(
    path: &Path,
    worktrees_root: &Path,
) -> Option<(String, Option<String>)> {
    let rel = path.strip_prefix(worktrees_root).ok()?;
    let mut comps = rel.components();
    let task = comps.next()?.as_os_str().to_string_lossy().into_owned();
    let repo = comps.next().map(|c| c.as_os_str().to_string_lossy().into_owned());
    Some((task, repo))
}

/// Is this path a git repo? Handles both plain repos (has `.git/`) and worktrees (has `.git` file).
pub fn is_git_repo(p: &Path) -> bool {
    let g = p.join(".git");
    g.is_dir() || g.is_file()
}

/// Real (non-symlink) child dirs of `dir` as `(utf8-name-or-None, path)`; missing dir yields empty.
pub fn list_child_dirs(dir: &Path) -> Vec<(Option<String>, PathBuf)> {
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new(); };
    rd.filter_map(Result::ok)
        .filter_map(|e| {
            let ft = e.file_type().ok()?;
            if ft.is_symlink() || !ft.is_dir() {
                return None;
            }
            let name = e.file_name().to_str().map(str::to_string);
            Some((name, e.path()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_slashes() {
        assert_eq!(sanitize_branch("feat/foo"), "feat-foo");
        assert_eq!(sanitize_branch("bug/nested/thing"), "bug-nested-thing");
        assert_eq!(sanitize_branch("no-slash"), "no-slash");
    }

    #[test]
    fn task_dir_layout() {
        let root = Path::new("/ws/foo");
        assert_eq!(task_dir(root, "feat/bar"), Path::new("/ws/foo/.worktrees/feat-bar"));
    }

    #[test]
    fn task_repo_under_extracts_task_and_repo() {
        let root = Path::new("/ws/proj/.worktrees");
        assert_eq!(
            task_repo_under(Path::new("/ws/proj/.worktrees/feat-foo/api/src"), root),
            Some(("feat-foo".into(), Some("api".into())))
        );
        assert_eq!(
            task_repo_under(Path::new("/ws/proj/.worktrees/bug-bar"), root),
            Some(("bug-bar".into(), None))
        );
        assert_eq!(task_repo_under(Path::new("/ws/proj/api/src"), root), None);
        assert_eq!(
            task_repo_under(Path::new("/ws/proj/.worktrees-backup/feat-foo/api"), root),
            None
        );
    }

    #[test]
    fn orphan_dirs_reports_only_what_git_doesnt_know() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let real = root.join(".worktrees/feat-x/api");
        let stray = root.join(".worktrees/feat-x/leftover");
        fs::create_dir_all(&real).unwrap();
        fs::create_dir_all(&stray).unwrap();

        let known = vec![Worktree {
            repo: "api".to_string(),
            path: real.clone(),
            branch: Some("feat/x".to_string()),
            task: Some("feat-x".to_string()),
        }];

        let orphans = orphan_dirs(root, &known);
        assert_eq!(orphans.len(), 1, "only the unregistered dir is an orphan: {orphans:?}");
        assert!(orphans[0].ends_with("leftover"), "{orphans:?}");
    }

    #[test]
    fn orphan_dirs_is_empty_when_git_knows_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let wt = root.join(".worktrees/feat-x/api");
        fs::create_dir_all(&wt).unwrap();

        let known = vec![Worktree {
            repo: "api".to_string(),
            path: wt,
            branch: None,
            task: Some("feat-x".to_string()),
        }];
        assert!(orphan_dirs(root, &known).is_empty());
    }

    #[test]
    fn list_child_dirs_skips_symlinks_and_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join("real")).unwrap();
        fs::write(root.join("plain.txt"), b"hi").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();

        let dirs: Vec<String> = list_child_dirs(root)
            .into_iter()
            .filter_map(|(n, _)| n)
            .collect();
        assert_eq!(dirs, vec!["real".to_string()]);
    }
}
