use anyhow::Result;
use std::path::Path;

use crate::config;
use crate::git;
use crate::workspace;

pub fn run(force: bool) -> Result<()> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;

    crate::ui::header("ws-clean", if force { "--force" } else { "" });

    let known = workspace::worktrees(&ws.root, &cfg);
    let mut by_task: std::collections::BTreeMap<String, Vec<&workspace::Worktree>> =
        std::collections::BTreeMap::new();
    for wt in &known {
        let Some(task) = wt.task.as_deref() else { continue };
        by_task.entry(task.to_string()).or_default().push(wt);
    }

    let mut removed = 0usize;
    let mut removable = 0usize;
    let mut kept = 0usize;

    for (task_name, worktrees) in &by_task {
        super::bail_if_interrupted()?;

        match audit(worktrees, task_name) {
            Verdict::Remove { branch, reason } => {
                crate::ui::clean_verdict(task_name, true, &reason, !force);
                if force {
                    // A hook failure is reported by its own step line; it must
                    // not abort the rest of the sweep.
                    let pairs: Vec<(String, std::path::PathBuf)> =
                        worktrees.iter().map(|c| (c.repo.clone(), c.path.clone())).collect();
                    let _ = super::teardown::remove_task(&ws.root, &cfg, &branch, &pairs)?;
                    removed += 1;
                } else {
                    removable += 1;
                }
            }
            Verdict::Keep { reason } => {
                crate::ui::clean_verdict(task_name, false, &reason, !force);
                kept += 1;
            }
        }
    }

    // Never removed — `ws` didn't necessarily put them there — but always
    // named, or they stay invisible to every op that reasons about worktrees.
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

/// A single dirty or unmerged repo keeps the whole task: never drop work that
/// isn't safely in the default branch. `branch` is carried over from the shared
/// enumeration rather than re-derived per worktree, so `ws list` and `ws clean`
/// can't disagree about it.
fn audit(worktrees: &[&workspace::Worktree], task_name: &str) -> Verdict {
    let mut resolved_branch: Option<String> = None;
    let mut unmerged_seen = false;

    for wt in worktrees {
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

    // With every worktree detached there is no branch to carry: the task name
    // is what `git branch -D` and task-dir pruning can still target.
    let branch = resolved_branch.unwrap_or_else(|| task_name.to_string());
    Verdict::Remove { branch, reason: "clean, no unmerged commits".into() }
}

/// False when no default branch can be resolved — keep rather than risk
/// deleting unmerged work.
fn is_ancestor_of_default(wt: &Path) -> bool {
    let Some(default) = git::default_ref(wt) else {
        return false;
    };
    // stderr is deliberately not nulled: a dangling `origin/HEAD` should still
    // say so on the terminal.
    git::git(wt)
        .args(["merge-base", "--is-ancestor", "HEAD", &default])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{audit, Verdict};
    use crate::workspace::Worktree;
    use std::path::{Path, PathBuf};

    /// Clean and on `main`, so `audit` always reaches the `Remove` arm and the
    /// only thing left to decide is *which branch* it names.
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

    fn candidate(path: &Path, branch: Option<&str>) -> Worktree {
        Worktree {
            repo: "api".into(),
            path: PathBuf::from(path),
            branch: branch.map(str::to_string),
            task: Some("t".into()),
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
        // The branch drives `git branch -D`, so which one wins is load-bearing.
        let repo = clean_repo();
        let p = repo.path();

        assert_eq!(
            removed_branch(audit(&[&candidate(p, Some("feat/x")), &candidate(p, Some("other"))], "t")),
            "feat/x",
        );
        assert_eq!(
            removed_branch(audit(&[&candidate(p, None), &candidate(p, Some("feat/x"))], "t")),
            "feat/x",
        );
        // The real branch keeps its slash; the sanitized dir name is only the
        // fallback, so `git branch -D` targets a ref that exists.
        assert_eq!(
            removed_branch(audit(&[&candidate(p, Some("feat/x"))], "feat-x")),
            "feat/x",
        );
    }

    #[test]
    fn an_all_detached_task_falls_back_to_the_task_name() {
        let repo = clean_repo();
        let verdict = audit(&[&candidate(repo.path(), None), &candidate(repo.path(), None)], "feat-x");
        assert_eq!(removed_branch(verdict), "feat-x");
    }
}
