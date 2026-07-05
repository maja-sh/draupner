use anyhow::{bail, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub struct Workspace {
    pub root: PathBuf,
}

/// Walk upward from CWD looking for a `.ws.json` file. Fails if none found.
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

/// Return all repo worktrees for a branch (repo name + path pairs).
pub fn all_worktrees_for(root: &Path, branch: &str) -> Vec<(String, PathBuf)> {
    list_child_dirs(&task_dir(root, branch))
        .into_iter()
        .filter_map(|(name, path)| name.map(|n| (n, path)))
        .collect()
}

/// Every task dir under `.worktrees/`. Used by `ws list` and `ws clean`.
pub fn all_task_dirs(root: &Path) -> Vec<PathBuf> {
    list_child_dirs(&root.join(".worktrees"))
        .into_iter()
        .map(|(_, path)| path)
        .collect()
}

/// If `path` is inside `<worktrees_root>/<task>[/<repo>/…]`, return the task dir
/// name and (when the path reaches into a repo) that repo name. Both paths are
/// expected to be canonical. The single source of truth for CWD→task/repo
/// inference used by `ws here`, `ws exec`, and `ws list`.
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

/// List real (non-symlink) child directories of `dir`. Returns pairs of
/// `(name_or_none_if_not_utf8, path)`. Broken entries and non-directories are
/// silently skipped. Errors on `read_dir` itself yield an empty vec.
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
        // Task dir itself — no repo component.
        assert_eq!(
            task_repo_under(Path::new("/ws/proj/.worktrees/bug-bar"), root),
            Some(("bug-bar".into(), None))
        );
        // Outside the tree, and a sibling-prefix collision, both yield None.
        assert_eq!(task_repo_under(Path::new("/ws/proj/api/src"), root), None);
        assert_eq!(
            task_repo_under(Path::new("/ws/proj/.worktrees-backup/feat-foo/api"), root),
            None
        );
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
