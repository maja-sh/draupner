//! The repeated `git -C <path> …` invocations. Policy — clean's audit verdict,
//! rm's safety scan, new's start-point choice — stays in the ops.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Public so ops that own their own git policy (new's `worktree add`, teardown's
/// removal ladder) still build the invocation here — a `-C` dropped at one of
/// those call sites runs the command against the wrong repo.
pub fn git(path: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(path);
    c
}

fn succeeds(c: &mut Command) -> bool {
    c.stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Empty on any failure — a non-zero exit, a missing git, a path that isn't a
/// repo — so callers reporting "nothing found" never have to distinguish those
/// from a genuinely empty answer.
pub fn stdout_lines(c: &mut Command) -> Vec<String> {
    match c.output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// Every worktree git has registered for `repo`. `branch` is `None` for a
/// detached HEAD, the first record is always the main checkout, and any failure
/// (not a repo, git missing, non-zero exit) reads as an empty vec.
pub fn worktree_list(repo: &Path) -> Vec<(PathBuf, Option<String>)> {
    let Ok(out) = git(repo).args(["worktree", "list", "--porcelain"]).output() else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    parse_worktree_list(&String::from_utf8_lossy(&out.stdout))
}

fn parse_worktree_list(text: &str) -> Vec<(PathBuf, Option<String>)> {
    let mut result = Vec::new();
    let mut cur_path: Option<PathBuf> = None;
    let mut cur_branch: Option<String> = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            // A new record begins — flush the previous one.
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

/// Checked before rendering a template over a `dest`: ws must never silently
/// dirty a committed file.
pub fn is_tracked(wt: &Path, rel: &str) -> bool {
    succeeds(git(wt).args(["ls-files", "--error-unmatch", rel]))
}

/// Append `entries` to the exclude file applying to `wt`, idempotently — how ws
/// keeps the artifacts it drops (symlinked deps, rendered templates) out of git.
/// Best-effort: a missing exclude path skips silently.
///
/// **Scope is the whole repo, not one worktree.** `info/` is in git's common list,
/// so `--git-path info/exclude` from a linked worktree resolves to the *main*
/// checkout's file — entries also hide matching untracked files in main and in
/// every sibling worktree, and nothing prunes them on `ws rm`. Genuine
/// per-worktree excludes would mean enabling `extensions.worktreeConfig` and
/// pointing a per-worktree `core.excludesFile` at our own file, which would
/// override the user's global excludes inside ws worktrees.
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

pub fn dirty_files(wt: &Path) -> Vec<String> {
    stdout_lines(git(wt).args(["status", "--porcelain"]))
}

pub fn is_dirty(wt: &Path) -> bool {
    !dirty_files(wt).is_empty()
}

pub fn ref_exists(path: &Path, refspec: &str) -> bool {
    succeeds(git(path).args(["rev-parse", "--verify", "--quiet", refspec]))
}

/// The repo's default branch as a start-point name (`origin/main`, `main`, …).
///
/// The ladder probes *fully-qualified* refspecs so a `main`/`master` **tag** (or
/// a stray `refs/main`) can't shadow the branch — short-name `rev-parse` would
/// resolve the tag first and branch new work off the wrong commit.
pub fn default_ref(path: &Path) -> Option<String> {
    // `origin/HEAD` outlives its target: after upstream renames its default
    // branch, `fetch --prune` deletes the remote-tracking ref but leaves the
    // symref pointing at it, and `symbolic-ref` doesn't dereference. So verify it
    // resolves and otherwise fall through to the ladder.
    if let Some(head) =
        symbolic_origin_head(path).filter(|h| ref_exists(path, &format!("refs/remotes/{h}")))
    {
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
        // The slash survives: callers need the real branch, not the sanitized
        // dir form, and the two can't be told apart afterwards.
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
