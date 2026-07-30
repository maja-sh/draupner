use anyhow::Result;
use std::path::Path;
use std::process::Command;

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
    let mut by_task: std::collections::BTreeMap<String, Vec<(String, std::path::PathBuf)>> =
        std::collections::BTreeMap::new();
    for wt in &known {
        let Some(task) = wt.task.as_deref() else { continue };
        by_task.entry(task.to_string()).or_default().push((wt.repo.clone(), wt.path.clone()));
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
                    let _ = super::teardown::remove_task(&ws.root, &cfg, &branch, worktrees)?;
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

/// Decide a task's fate from its worktrees. A task is removed only when every
/// worktree is clean *and* carries no commits missing from the default branch
/// (covers both "merged" and "never committed"). Any dirty or unmerged repo
/// keeps the whole task — we never drop work that isn't safely in the default.
fn audit(worktrees: &[(String, std::path::PathBuf)], task_name: &str) -> Verdict {
    let mut resolved_branch: Option<String> = None;
    let mut unmerged_seen = false;

    for (_repo, wt) in worktrees {
        // Dirty dominates — a single dirty repo keeps the whole task, and its
        // ancestry/branch no longer matter, so stop scanning.
        if git::is_dirty(wt) {
            return Verdict::Keep { reason: "dirty".into() };
        }
        if resolved_branch.is_none() {
            resolved_branch = current_branch(wt);
        }
        if !is_ancestor_of_default(wt) {
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

fn current_branch(wt: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(wt)
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None; // detached HEAD
    }
    let b = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!b.is_empty()).then_some(b)
}

/// True when the worktree's HEAD is already contained in the default branch —
/// i.e. dropping it loses nothing. False when no default can be resolved
/// (conservative: keep rather than risk deleting unmerged work).
fn is_ancestor_of_default(wt: &Path) -> bool {
    let Some(default) = git::default_ref(wt) else {
        return false;
    };
    Command::new("git")
        .arg("-C")
        .arg(wt)
        .args(["merge-base", "--is-ancestor", "HEAD", &default])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
