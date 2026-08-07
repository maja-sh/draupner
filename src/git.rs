//! Small git-plumbing helpers shared across ops; ops keep their own policy and this module owns the repeated `git -C <path>` invocations.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn git(path: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(path);
    c
}

/// Every worktree git has registered for `repo`, as `(path, branch)`; `branch` is `None` for a detached HEAD, the first record is the main checkout, and any failure yields an empty vec.
pub fn worktree_list(repo: &Path) -> Vec<(PathBuf, Option<String>)> {
    let Ok(out) = git(repo).args(["worktree", "list", "--porcelain"]).output() else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    parse_worktree_list(&String::from_utf8_lossy(&out.stdout))
}

/// Parse `git worktree list --porcelain`; split from the `Command` call so the record framing is testable without a repo.
fn parse_worktree_list(text: &str) -> Vec<(PathBuf, Option<String>)> {
    let mut result = Vec::new();
    let mut cur_path: Option<PathBuf> = None;
    let mut cur_branch: Option<String> = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            if let Some(path) = cur_path.take() {
                result.push((path, cur_branch.take()));
            }
            cur_path = Some(PathBuf::from(p));
            cur_branch = None;
        } else if let Some(b) = line.strip_prefix("branch ") {
            cur_branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
        }
    }
    if let Some(path) = cur_path {
        result.push((path, cur_branch));
    }
    result
}

/// Whether `rel` is tracked by git in the worktree; checked before rendering a template so we never silently dirty a committed file.
pub fn is_tracked(wt: &Path, rel: &str) -> bool {
    git(wt)
        .args(["ls-files", "--error-unmatch", rel])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Append `entries` to the worktree's `info/exclude` idempotently, resolving the path via `git rev-parse --git-path` so linked-worktree gitdir pointers are followed.
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

/// `git status --porcelain` lines; empty on a clean tree or any failure.
pub fn dirty_files(wt: &Path) -> Vec<String> {
    match git(wt).args(["status", "--porcelain"]).output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

pub fn is_dirty(wt: &Path) -> bool {
    !dirty_files(wt).is_empty()
}

/// Whether `refspec` resolves in the repo; accepts full refs or short names.
pub fn ref_exists(path: &Path, refspec: &str) -> bool {
    git(path)
        .args(["rev-parse", "--verify", "--quiet", refspec])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The repo's default branch as a start-point name (`origin/main`, `main`, …), or `None`; probes fully-qualified refspecs so a `main`/`master` tag can't shadow the branch.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_list_parses_records_and_strips_the_branch_ref() {
        let out = "worktree /home/u/ws/api\nHEAD abc123\nbranch refs/heads/main\n\n\
                   worktree /home/u/ws/.worktrees/feat-x/api\nHEAD def456\nbranch refs/heads/feat/x\n\n";
        let got = parse_worktree_list(out);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], (PathBuf::from("/home/u/ws/api"), Some("main".to_string())));
        // Branch keeps its slash; only `refs/heads/` is stripped.
        assert_eq!(
            got[1],
            (PathBuf::from("/home/u/ws/.worktrees/feat-x/api"), Some("feat/x".to_string()))
        );
    }

    #[test]
    fn worktree_list_reports_a_detached_head_as_no_branch() {
        let out = "worktree /home/u/ws/api\nHEAD abc\nbranch refs/heads/main\n\n\
                   worktree /home/u/ws/.worktrees/t/api\nHEAD def\ndetached\n\n";
        let got = parse_worktree_list(out);
        assert_eq!(got.len(), 2, "the trailing record must still be flushed");
        assert_eq!(got[1].1, None);
    }

    #[test]
    fn worktree_list_is_empty_for_empty_output() {
        assert!(parse_worktree_list("").is_empty());
    }
}
