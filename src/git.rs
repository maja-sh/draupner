//! Small git-plumbing helpers shared across ops. Ops keep their own *policy*
//! (clean's audit verdict, rm's safety scan, new's start-point choice); this
//! module owns the repeated `git -C <path> …` invocations they were each
//! re-deriving.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// `git -C <path>` with no other args set yet. Public because ops that own their
/// own git *policy* (new's `worktree add`, teardown's removal ladder) still route
/// the invocation itself through here — a `-C` dropped at one of those call sites
/// runs the command against the wrong repo.
pub fn git(path: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(path);
    c
}

/// Run `c` and report whether it exited cleanly; output discarded. For the
/// probe-shaped commands whose exit code *is* the answer.
fn succeeds(c: &mut Command) -> bool {
    c.stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run `c` and return its stdout as lines. Empty on any failure — a non-zero
/// exit, a missing git, a path that isn't a repo — so callers reporting "nothing
/// found" never have to distinguish those from a genuinely empty answer.
pub fn stdout_lines(c: &mut Command) -> Vec<String> {
    match c.output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// Every worktree git has registered for `repo`, as `(path, branch)`. `branch` is
/// `None` for a detached HEAD. The first record is always the main checkout.
///
/// This is the authoritative answer to "what worktrees exist" — a directory under
/// `.worktrees/` is *not* a worktree (it can be the residue of a failed `ws rm`, a
/// bare `git worktree remove`, or a hand-made copy), and git additionally knows
/// about worktrees living outside `.worktrees/` entirely. An empty vec on any
/// failure: not a repo, git missing, or a non-zero exit.
pub fn worktree_list(repo: &Path) -> Vec<(PathBuf, Option<String>)> {
    let Ok(out) = git(repo).args(["worktree", "list", "--porcelain"]).output() else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    parse_worktree_list(&String::from_utf8_lossy(&out.stdout))
}

/// Parse `git worktree list --porcelain`: blank-line-separated records, each
/// opening with `worktree <path>` and optionally carrying `branch <ref>`. Split
/// out from the `Command` call so the record framing is testable without a repo.
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

/// Whether `rel` (a worktree-relative path) is tracked by git in the worktree.
/// Used before rendering a template to a `dest` so we never silently dirty a
/// committed file.
pub fn is_tracked(wt: &Path, rel: &str) -> bool {
    succeeds(git(wt).args(["ls-files", "--error-unmatch", rel]))
}

/// Append `entries` to the git exclude file (`info/exclude`) that applies to
/// `wt`, idempotently. Resolves the real exclude path via `git rev-parse
/// --git-path` so it works through a linked worktree's `.git` gitdir pointer.
/// Best-effort on lookup — a missing exclude path just skips silently. This is
/// how `ws` keeps the artifacts it drops (symlinked deps, rendered templates)
/// out of git.
///
/// **Scope is the whole repo, not one worktree.** `info/` is in git's common
/// list (only `info/sparse-checkout` is per-worktree), so `--git-path
/// info/exclude` from a linked worktree resolves to the *main* checkout's
/// `.git/info/exclude` — entries written for a worktree also hide matching
/// untracked files in main and in every sibling worktree, and nothing prunes
/// them on `ws rm`. Genuine per-worktree excludes would mean enabling the
/// `extensions.worktreeConfig` repo format extension and pointing a per-worktree
/// `core.excludesFile` at our own file, which would also override the user's
/// global excludes file inside ws worktrees — not something to turn on silently.
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
    stdout_lines(git(wt).args(["status", "--porcelain"]))
}

/// Whether the worktree has uncommitted changes (tracked or untracked).
pub fn is_dirty(wt: &Path) -> bool {
    !dirty_files(wt).is_empty()
}

/// Whether `refspec` resolves in the repo (`rev-parse --verify`). Accepts full
/// refs (`refs/heads/main`) or short names (`origin/main`).
pub fn ref_exists(path: &Path, refspec: &str) -> bool {
    succeeds(git(path).args(["rev-parse", "--verify", "--quiet", refspec]))
}

/// The repo's default branch as a start-point name (`origin/main`, `main`, …),
/// or `None` when none can be resolved. Prefers `origin/HEAD`, then falls back
/// through the conventional `main`/`master` ladder (remote-tracking first).
///
/// The ladder probes *fully-qualified* refspecs so a `main`/`master` **tag**
/// (or a stray `refs/main`) can't shadow the branch — short-name `rev-parse`
/// would resolve the tag first and branch new work off the wrong commit.
pub fn default_ref(path: &Path) -> Option<String> {
    // `origin/HEAD` is only usable if it still *resolves*. When upstream renames
    // its default branch, a `git fetch --prune` deletes `refs/remotes/origin/master`
    // but leaves the `origin/HEAD` symref pointing at it; `symbolic-ref` doesn't
    // dereference, so it keeps reporting the dead branch. Handing that to
    // `git worktree add` failed the whole op ("not a valid object name") for a
    // repo whose ladder below resolves `origin/main` fine — so probe, then fall
    // through instead of trusting the symref.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_list_parses_records_and_strips_the_branch_ref() {
        // Real porcelain shape: blank-line-separated records, main checkout first.
        let out = "worktree /home/u/ws/api\nHEAD abc123\nbranch refs/heads/main\n\n\
                   worktree /home/u/ws/.worktrees/feat-x/api\nHEAD def456\nbranch refs/heads/feat/x\n\n";
        let got = parse_worktree_list(out);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], (PathBuf::from("/home/u/ws/api"), Some("main".to_string())));
        // The branch keeps its slash — only the `refs/heads/` prefix is stripped,
        // so the caller gets the real name rather than the sanitized dir form.
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
