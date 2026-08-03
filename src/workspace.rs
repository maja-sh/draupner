use anyhow::{bail, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub struct Workspace {
    pub root: PathBuf,
}

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

pub fn sanitize_branch(b: &str) -> String {
    b.replace('/', "-")
}

/// The name every namespace ws keys off a worktree shares (mutagen sessions,
/// host-process pid/log files). Built once here rather than agreed on by
/// convention: the namespaces are documented as reading alike and one of them is
/// a user-facing contract. See [`crate::mutagen::session_name`] and
/// [`crate::supervise::proc_name`].
pub fn ws_name(branch: &str, repo: &str) -> String {
    format!("ws-{}-{}", sanitize_branch(branch), repo)
}

pub fn task_dir(root: &Path, branch: &str) -> PathBuf {
    root.join(".worktrees").join(sanitize_branch(branch))
}

/// The `.worktrees` dir, canonicalized so a path derived from a canonical cwd
/// strips against it. Callers comparing a real filesystem path to the worktrees
/// tree must come through here rather than joining the literal.
pub fn worktrees_root(root: &Path) -> PathBuf {
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf()).join(".worktrees")
}

pub fn worktree_for(root: &Path, branch: &str, repo: &str) -> Option<PathBuf> {
    let p = task_dir(root, branch).join(repo);
    p.is_dir().then_some(p)
}

pub fn all_worktrees_for(root: &Path, branch: &str) -> Vec<(String, PathBuf)> {
    list_child_dirs(&task_dir(root, branch))
        .into_iter()
        .filter_map(|(name, path)| name.map(|n| (n, path)))
        .collect()
}

/// A *filesystem* view, not the authoritative one — [`worktrees`] is. Exists for
/// [`orphan_dirs`], which compares the two.
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
    /// `None` for a detached HEAD.
    pub branch: Option<String>,
    /// The `.worktrees/<task>` dir name; `None` for a worktree the user created
    /// elsewhere by hand (`ws list` renders those as "(unmanaged)").
    pub task: Option<String>,
}

/// Every task worktree across the workspace's configured repos — **the single
/// definition of "a worktree exists"**, sourced from git rather than from a scan
/// of `.worktrees/` (see ARCHITECTURE.md; a directory is not a worktree, and git
/// sees ones living outside the tree). The main checkout is excluded.
pub fn worktrees(root: &Path, cfg: &crate::config::WsConfig) -> Vec<Worktree> {
    let worktrees_root = worktrees_root(root);

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
            // The repo component is implied by which repo reported the worktree.
            let task = task_repo_under(&canon, &worktrees_root).map(|(task, _)| task);
            out.push(Worktree { repo: repo.to_string(), path, branch, task });
        }
    }
    out
}

/// Directories under `.worktrees/` that git does **not** know as worktrees —
/// leftovers of a failed removal, a bare `git worktree remove`, a copied
/// directory. Reported (by `ws clean`), never acted on: acting on one, syncing it
/// to a server say, is silent corruption.
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

/// If `path` is inside `<worktrees_root>/<task>[/<repo>/…]`, the task dir name
/// and (when the path reaches into a repo) that repo name. Both paths must
/// already be canonical.
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

/// A linked worktree's `.git` is a file, not a directory — hence both checks.
pub fn is_git_repo(p: &Path) -> bool {
    let g = p.join(".git");
    g.is_dir() || g.is_file()
}

/// Child directories as `(name, path)`, where the name is `None` when it isn't
/// UTF-8. Symlinked directories are deliberately not included.
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
        // The second case is a sibling-prefix collision, not just an outside path.
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
