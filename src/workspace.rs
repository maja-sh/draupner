use anyhow::{bail, Result};
use std::fs;
use std::path::{Path, PathBuf};

#[allow(dead_code)]
pub struct Workspace {
    pub root: PathBuf,
}

/// Walk upward from CWD looking for a `.ws.json` file. Fails if none found.
#[allow(dead_code)]
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
#[allow(dead_code)]
pub fn sanitize_branch(b: &str) -> String {
    b.replace('/', "-")
}

/// `<root>/.worktrees/<sanitized-branch>/`
#[allow(dead_code)]
pub fn task_dir(root: &Path, branch: &str) -> PathBuf {
    root.join(".worktrees").join(sanitize_branch(branch))
}

/// `<root>/.worktrees/<sanitized-branch>/<repo>/` if the worktree exists on disk.
#[allow(dead_code)]
pub fn worktree_for(root: &Path, branch: &str, repo: &str) -> Option<PathBuf> {
    let p = task_dir(root, branch).join(repo);
    p.is_dir().then_some(p)
}

/// Return all repo worktrees for a branch (repo name + path pairs).
#[allow(dead_code)]
pub fn all_worktrees_for(root: &Path, branch: &str) -> Vec<(String, PathBuf)> {
    list_child_dirs(&task_dir(root, branch))
        .into_iter()
        .filter_map(|(name, path)| name.map(|n| (n, path)))
        .collect()
}

/// Every task dir under `.worktrees/`. Used by `ws list` and `ws clean`.
#[allow(dead_code)]
pub fn all_task_dirs(root: &Path) -> Vec<PathBuf> {
    list_child_dirs(&root.join(".worktrees"))
        .into_iter()
        .map(|(_, path)| path)
        .collect()
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
