//! Small git-plumbing helpers shared across ops. Ops keep their own *policy*
//! (clean's audit verdict, rm's safety scan, new's start-point choice); this
//! module owns the repeated `git -C <path> …` invocations they were each
//! re-deriving.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// `git -C <path>` with no other args set yet.
fn git(path: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(path);
    c
}

/// Whether `rel` (a worktree-relative path) is tracked by git in the worktree.
/// Used before rendering a template to a `dest` so we never silently dirty a
/// committed file.
pub fn is_tracked(wt: &Path, rel: &str) -> bool {
    git(wt)
        .args(["ls-files", "--error-unmatch", rel])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Append `entries` to the worktree's git exclude file (`info/exclude`),
/// idempotently. Resolves the real exclude path via `git rev-parse --git-path`
/// so it works through a linked worktree's `.git` gitdir pointer. Best-effort on
/// lookup — a missing exclude path just skips silently. This is how `ws` keeps
/// the artifacts it drops (symlinked deps, rendered templates) out of git.
pub fn add_worktree_excludes(wt: &Path, entries: &[String]) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let out = git(wt)
        .args(["rev-parse", "--git-path", "info/exclude"])
        .output()
        .context("spawn git rev-parse --git-path")?;
    if !out.status.success() {
        return Ok(());
    }
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if raw.is_empty() {
        return Ok(());
    }
    let rel = PathBuf::from(&raw);
    let path = if rel.is_absolute() { rel } else { wt.join(rel) };

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let present: std::collections::HashSet<&str> = existing.lines().map(str::trim).collect();

    let mut content = existing.clone();
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    let mut added = false;
    for e in entries {
        if !present.contains(e.as_str()) {
            content.push_str(e);
            content.push('\n');
            added = true;
        }
    }
    if added {
        std::fs::write(&path, content).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
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
///
/// The ladder probes *fully-qualified* refspecs so a `main`/`master` **tag**
/// (or a stray `refs/main`) can't shadow the branch — short-name `rev-parse`
/// would resolve the tag first and branch new work off the wrong commit.
pub fn default_ref(path: &Path) -> Option<String> {
    if let Some(head) = symbolic_origin_head(path) {
        return Some(head);
    }
    [
        ("refs/remotes/origin/main", "origin/main"),
        ("refs/remotes/origin/master", "origin/master"),
        ("refs/heads/main", "main"),
        ("refs/heads/master", "master"),
    ]
    .into_iter()
    .find(|(refspec, _)| ref_exists(path, refspec))
    .map(|(_, name)| name.to_string())
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
