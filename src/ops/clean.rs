use anyhow::Result;
use std::path::Path;

use crate::config;
use crate::git;
use crate::workspace;

pub fn run(force: bool) -> Result<()> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;

    crate::ui::header("ws-clean", if force { "--force" } else { "" });

    // The audit runs over what **git** calls a worktree (the shared enumeration),
    // grouped back into tasks. The filesystem residue git doesn't know about is a
    // separate question, reported below rather than audited — `audit` asks git
    // things (dirty? unmerged commits?) that a stray directory can't answer.
    let known = workspace::worktrees(&ws.root, &cfg);
    let mut by_task: std::collections::BTreeMap<String, Vec<Candidate>> =
        std::collections::BTreeMap::new();
    for wt in &known {
        let Some(task) = wt.task.as_deref() else { continue };
        by_task.entry(task.to_string()).or_default().push(Candidate {
            repo: wt.repo.clone(),
            path: wt.path.clone(),
            branch: wt.branch.clone(),
        });
    }

    let mut removed = 0usize;
    let mut removable = 0usize;
    let mut kept = 0usize;

    for (task_name, worktrees) in &by_task {
        super::bail_if_interrupted()?;
        let task_name = task_name.clone();

        match audit(worktrees, &task_name) {
            Verdict::Remove { branch, reason } => {
                crate::ui::clean_verdict(&task_name, true, &reason, !force);
                if force {
                    // Reuse the shared teardown. A hook failure here is reported
                    // by the step line; we don't abort the whole clean over it.
                    let pairs: Vec<(String, std::path::PathBuf)> =
                        worktrees.iter().map(|c| (c.repo.clone(), c.path.clone())).collect();
                    let _ = super::teardown::remove_task(&ws.root, &cfg, &branch, &pairs)?;
                    removed += 1;
                } else {
                    removable += 1;
                }
            }
            Verdict::Keep { reason } => {
                crate::ui::clean_verdict(&task_name, false, &reason, !force);
                kept += 1;
            }
        }
    }

    // Directories under `.worktrees/` that git doesn't know as worktrees. Never
    // removed — `ws` didn't necessarily put them there and can't judge what's in
    // them — but always named, because the alternative is what they cost you
    // otherwise: invisible to every op that reasons about worktrees, yet real
    // enough on disk for a filesystem walk to pick up and act on.
    let orphans = workspace::orphan_dirs(&ws.root, &known);
    if !orphans.is_empty() {
        crate::ui::note(&format!(
            "{} director{} under .worktrees/ {} not registered with git — leftovers from an \
             interrupted removal, or copied by hand. `ws` ignores them; remove by hand once \
             you've checked them:",
            orphans.len(),
            if orphans.len() == 1 { "y" } else { "ies" },
            if orphans.len() == 1 { "is" } else { "are" },
        ));
        crate::ui::command_block(
            &orphans.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        );
    }

    if removed + removable + kept == 0 {
        if orphans.is_empty() {
            crate::ui::clean_empty();
        }
        return Ok(());
    }

    if force {
        crate::ui::summary_ok(&[format!("Removed {removed}, kept {kept}")]);
    } else if removable > 0 {
        crate::ui::summary_ok(&[format!("{removable} removable, {kept} kept")]);
        crate::ui::nudge("ws clean --force to remove");
    } else {
        crate::ui::summary_ok(&[format!("Nothing to remove, {kept} kept")]);
    }
    Ok(())
}

enum Verdict {
    Remove { branch: String, reason: String },
    Keep { reason: String },
}

/// One worktree up for audit. `branch` is carried over from the shared
/// enumeration rather than re-derived per worktree: `workspace::worktrees` reads
/// it straight out of `git worktree list --porcelain`, and asking git a second
/// time (once per worktree) is both a subprocess and a second answer to a
/// question that already has a single source of truth.
struct Candidate {
    repo: String,
    path: std::path::PathBuf,
    /// The checked-out branch; `None` for a detached HEAD.
    branch: Option<String>,
}

/// Decide a task's fate from its worktrees. A task is removed only when every
/// worktree is clean *and* carries no commits missing from the default branch
/// (covers both "merged" and "never committed"). Any dirty or unmerged repo
/// keeps the whole task — we never drop work that isn't safely in the default.
fn audit(worktrees: &[Candidate], task_name: &str) -> Verdict {
    let mut resolved_branch: Option<String> = None;
    let mut unmerged_seen = false;

    for wt in worktrees {
        // Dirty dominates — a single dirty repo keeps the whole task, and its
        // ancestry/branch no longer matter, so stop scanning.
        if git::is_dirty(&wt.path) {
            return Verdict::Keep { reason: "dirty".into() };
        }
        if resolved_branch.is_none() {
            resolved_branch = wt.branch.clone();
        }
        if !is_ancestor_of_default(&wt.path) {
            unmerged_seen = true;
        }
    }

    if unmerged_seen {
        return Verdict::Keep { reason: "unmerged commits".into() };
    }

    // Fall back to the (sanitized) task name so `git branch -D` / task-dir
    // pruning still target the right place when every worktree is detached.
    let branch = resolved_branch.unwrap_or_else(|| task_name.to_string());
    Verdict::Remove { branch, reason: "clean, no unmerged commits".into() }
}

/// True when the worktree's HEAD is already contained in the default branch —
/// i.e. dropping it loses nothing. False when no default can be resolved
/// (conservative: keep rather than risk deleting unmerged work).
fn is_ancestor_of_default(wt: &Path) -> bool {
    let Some(default) = git::default_ref(wt) else {
        return false;
    };
    // stderr is deliberately left alone rather than nulled: a default that
    // doesn't resolve (a dangling `origin/HEAD`) still says so on the terminal.
    git::git(wt)
        .args(["merge-base", "--is-ancestor", "HEAD", &default])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{audit, Candidate, Verdict};
    use std::path::{Path, PathBuf};

    /// A clean repo on `main` with one commit: `is_dirty` is false and HEAD is an
    /// ancestor of the default, so `audit` reaches the `Remove` arm and the only
    /// thing left to decide is *which branch* it names.
    fn clean_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(tmp.path())
                .args(args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
        };
        run(&["init", "-q"]);
        run(&["symbolic-ref", "HEAD", "refs/heads/main"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(tmp.path().join("README.md"), "x").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "init"]);
        tmp
    }

    fn candidate(path: &Path, branch: Option<&str>) -> Candidate {
        Candidate {
            repo: "api".into(),
            path: PathBuf::from(path),
            branch: branch.map(str::to_string),
        }
    }

    fn removed_branch(verdict: Verdict) -> String {
        match verdict {
            Verdict::Remove { branch, .. } => branch,
            Verdict::Keep { reason } => panic!("expected Remove, got Keep({reason})"),
        }
    }

    #[test]
    fn the_removed_branch_is_the_first_one_the_enumeration_reported() {
        // The branch drives `git branch -D`, so which one wins is load-bearing:
        // the first worktree that has one, detached worktrees skipped. It comes
        // from the shared enumeration, not from asking the worktree's HEAD again.
        let repo = clean_repo();
        let p = repo.path();

        assert_eq!(
            removed_branch(audit(&[candidate(p, Some("feat/x")), candidate(p, Some("other"))], "t")),
            "feat/x",
        );
        // A detached worktree contributes no branch and doesn't block a later one.
        assert_eq!(
            removed_branch(audit(&[candidate(p, None), candidate(p, Some("feat/x"))], "t")),
            "feat/x",
        );
        // The real branch keeps its slash — the sanitized task dir name is only
        // the fallback, so `git branch -D` targets the ref that exists.
        assert_eq!(
            removed_branch(audit(&[candidate(p, Some("feat/x"))], "feat-x")),
            "feat/x",
        );
    }

    #[test]
    fn an_all_detached_task_falls_back_to_the_task_name() {
        let repo = clean_repo();
        let verdict = audit(&[candidate(repo.path(), None), candidate(repo.path(), None)], "feat-x");
        assert_eq!(removed_branch(verdict), "feat-x");
    }
}
