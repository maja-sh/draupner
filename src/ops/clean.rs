use anyhow::Result;
use std::path::Path;
use std::process::Command;

use crate::config;
use crate::workspace;

pub fn run() -> Result<()> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;

    crate::ui::header("ws-clean", "");

    let mut task_dirs = workspace::all_task_dirs(&ws.root);
    task_dirs.sort();

    let mut removed = 0usize;
    let mut kept = 0usize;
    let mut any = false;

    for task_dir in &task_dirs {
        super::bail_if_interrupted()?;

        let task_name = match task_dir.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let worktrees = workspace::all_worktrees_for(&ws.root, &task_name);
        if worktrees.is_empty() {
            continue;
        }
        any = true;

        match audit(&worktrees, &task_name) {
            Verdict::Remove { branch, reason } => {
                crate::ui::clean_verdict(&task_name, true, &reason);
                // Reuse the shared teardown. A hook failure here is reported by
                // the step line; we don't abort the whole clean over it.
                let _ = super::teardown::remove_task(&ws.root, &cfg, &branch, &worktrees)?;
                removed += 1;
            }
            Verdict::Keep { reason } => {
                crate::ui::clean_verdict(&task_name, false, &reason);
                kept += 1;
            }
        }
    }

    if !any {
        crate::ui::clean_empty();
        return Ok(());
    }

    crate::ui::summary_ok(&[format!("Removed {removed}, kept {kept}")]);
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
    let mut dirty_seen = false;
    let mut unmerged_seen = false;

    for (_repo, wt) in worktrees {
        if is_dirty(wt) {
            dirty_seen = true;
        }
        if resolved_branch.is_none() {
            resolved_branch = current_branch(wt);
        }
        if !is_ancestor_of_default(wt) {
            unmerged_seen = true;
        }
    }

    // Dirty dominates unmerged in the reason we surface — either keeps the task.
    if dirty_seen {
        return Verdict::Keep { reason: "dirty".into() };
    }
    if unmerged_seen {
        return Verdict::Keep { reason: "unmerged commits".into() };
    }

    // Fall back to the (sanitized) task name so `git branch -D` / task-dir
    // pruning still target the right place when every worktree is detached.
    let branch = resolved_branch.unwrap_or_else(|| task_name.to_string());
    Verdict::Remove { branch, reason: "clean, no unmerged commits".into() }
}

fn is_dirty(wt: &Path) -> bool {
    let out = Command::new("git")
        .arg("-C")
        .arg(wt)
        .args(["status", "--porcelain"])
        .output();
    match out {
        Ok(out) if out.status.success() => !out.stdout.is_empty(),
        _ => false,
    }
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
    let Some(default) = default_ref(wt) else {
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

fn default_ref(wt: &Path) -> Option<String> {
    if let Some(head) = symbolic_origin_head(wt) {
        return Some(head);
    }
    for candidate in ["origin/main", "origin/master", "main", "master"] {
        if ref_exists(wt, candidate) {
            return Some(candidate.to_string());
        }
    }
    None
}

fn symbolic_origin_head(wt: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(wt)
        .args(["symbolic-ref", "refs/remotes/origin/HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let full = String::from_utf8_lossy(&out.stdout).trim().to_string();
    full.strip_prefix("refs/remotes/").map(String::from)
}

fn ref_exists(wt: &Path, refname: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(wt)
        .args(["rev-parse", "--verify", "--quiet", refname])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
