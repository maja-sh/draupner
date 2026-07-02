use anyhow::{bail, Result};
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
    let td = task_dir(root, branch);
    let Ok(rd) = std::fs::read_dir(&td) else { return Vec::new(); };
    rd.filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            Some((name, e.path()))
        })
        .collect()
}

/// Every task dir under `.worktrees/`. Used by `ws list` and `ws clean`.
#[allow(dead_code)]
pub fn all_task_dirs(root: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(root.join(".worktrees")) else { return Vec::new(); };
    rd.filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .map(|e| e.path())
        .collect()
}

/// Is this path a git repo? Handles both plain repos (has `.git/`) and worktrees (has `.git` file).
#[allow(dead_code)]
pub fn is_git_repo(p: &Path) -> bool {
    let g = p.join(".git");
    g.is_dir() || g.is_file()
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
}
