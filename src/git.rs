//! Small git-plumbing helpers shared across ops. Ops keep their own *policy*
//! (clean's audit verdict, rm's safety scan, new's start-point choice); this
//! module owns the repeated `git -C <path> …` invocations they were each
//! re-deriving.

use std::path::Path;
use std::process::{Command, Stdio};

/// `git -C <path>` with no other args set yet.
fn git(path: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(path);
    c
}

/// `git status --porcelain` lines. Empty on a clean tree, on error, or when the
/// path isn't a git repo.
pub fn dirty_files(wt: &Path) -> Vec<String> {
    match git(wt).args(["status", "--porcelain"]).output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether the worktree has uncommitted changes (tracked or untracked).
pub fn is_dirty(wt: &Path) -> bool {
    !dirty_files(wt).is_empty()
}

/// Whether `refspec` resolves in the repo (`rev-parse --verify`). Accepts full
/// refs (`refs/heads/main`) or short names (`origin/main`).
pub fn ref_exists(path: &Path, refspec: &str) -> bool {
    git(path)
        .args(["rev-parse", "--verify", "--quiet", refspec])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The repo's default branch as a start-point name (`origin/main`, `main`, …),
/// or `None` when none can be resolved. Prefers `origin/HEAD`, then falls back
/// through the conventional `main`/`master` ladder (remote-tracking first).
pub fn default_ref(path: &Path) -> Option<String> {
    if let Some(head) = symbolic_origin_head(path) {
        return Some(head);
    }
    ["origin/main", "origin/master", "main", "master"]
        .into_iter()
        .find(|c| ref_exists(path, c))
        .map(String::from)
}

/// `git symbolic-ref refs/remotes/origin/HEAD`, stripped to a short name
/// (`origin/main`). `None` when origin has no HEAD symref.
fn symbolic_origin_head(path: &Path) -> Option<String> {
    let out = git(path)
        .args(["symbolic-ref", "refs/remotes/origin/HEAD"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let full = String::from_utf8_lossy(&out.stdout).trim().to_string();
    full.strip_prefix("refs/remotes/").map(String::from)
}
