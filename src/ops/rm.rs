use anyhow::Result;
use std::path::Path;
use std::process::Command;

use crate::config::{self, WsConfig};
use crate::hooks;
use crate::workspace;

pub fn run(branch: &str, force: bool) -> Result<()> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;

    let sanitized = workspace::sanitize_branch(branch);
    let task_dir = workspace::task_dir(&ws.root, branch);
    let worktrees = workspace::all_worktrees_for(&ws.root, branch);

    if worktrees.is_empty() {
        return Err(super::preflight(format!(
            "no worktrees for {branch}; nothing to remove"
        )));
    }

    crate::ui::header("ws-rm", branch);

    // Safety scan — surface dirty / unpushed state. Blocks teardown unless
    // `--force`. With `--force`, warnings still print (we're overriding them).
    let mut blocking = false;
    for (repo, wt) in &worktrees {
        let dirty = dirty_files(wt);
        if !dirty.is_empty() {
            crate::ui::warn(&format!("{repo}: uncommitted changes"), &dirty);
            blocking = true;
        }
        let unpushed = unpushed_commits(wt);
        if !unpushed.is_empty() {
            crate::ui::warn(&format!("{repo}: unpushed commits"), &unpushed);
            blocking = true;
        }
    }
    if blocking && !force {
        return Err(super::op_failure(format!(
            "worktree(s) not clean; re-run `ws rm {branch} --force` to remove anyway"
        )));
    }

    let mut hook_failed = false;

    for (repo, wt) in &worktrees {
        // Resource teardown hooks — best-effort. A failing hook is reported
        // but doesn't stop removal (the user wants the worktree gone).
        for event in ["down", "post-remove"] {
            let hook_override = repo_hook_override(&cfg, repo, event);
            if let Some(rel) = hooks::lookup(&ws.root, repo, event, hook_override) {
                let step = crate::ui::step(repo, &format!("{event} hook"));
                match hooks::run(&ws.root, &rel, wt, repo, &sanitized, event) {
                    Ok(()) => step.ok(),
                    Err(e) => {
                        step.fail(&format!("{e}"));
                        hook_failed = true;
                    }
                }
            }
        }

        // Drop symlinked deps first so `git worktree remove` sees a clean tree.
        remove_if_symlink(&wt.join("node_modules"));
        remove_if_symlink(&wt.join("vendor"));

        let main = ws.root.join(repo);
        let step = crate::ui::step(repo, "worktree removed");
        match remove_worktree(&main, wt) {
            Ok(()) => step.ok(),
            Err(e) => {
                step.fail("git worktree remove failed");
                return Err(super::op_failure(format!(
                    "{e}; try `git worktree prune` in {}",
                    main.display()
                )));
            }
        }

        // Delete the local branch — best-effort. A branch that's gone (merged
        // and pruned) or checked out elsewhere just leaves this a no-op.
        delete_branch(&main, branch);
    }

    // Best-effort: drop the now-empty task dir. Leftover files (a stray
    // .DS_Store, an unmanaged sibling) make this a silent no-op.
    let _ = std::fs::remove_dir(&task_dir);

    if hook_failed {
        return Err(super::op_failure(format!(
            "removed worktrees for {branch}, but one or more hooks failed (see above)"
        )));
    }

    crate::ui::summary_ok(&[format!("Removed worktrees for {branch}")]);
    crate::ui::nudge(&format!("ws new <repos> {branch} to recreate"));
    Ok(())
}

fn repo_hook_override<'a>(cfg: &'a WsConfig, repo: &str, event: &str) -> Option<&'a str> {
    cfg.repos
        .iter()
        .find(|r| r.name() == repo)
        .and_then(|r| r.hook_override(event))
}

fn dirty_files(wt: &Path) -> Vec<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(wt)
        .args(["status", "--porcelain"])
        .output();
    match out {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// Commits present locally but not on the tracked upstream. Empty when there's
/// no upstream configured (can't be "unpushed" against nothing) or on error.
fn unpushed_commits(wt: &Path) -> Vec<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(wt)
        .args(["log", "@{u}..HEAD", "--oneline"])
        .output();
    match out {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn remove_if_symlink(p: &Path) {
    if let Ok(meta) = p.symlink_metadata() {
        if meta.file_type().is_symlink() {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Remove a worktree, retrying with `--force` when a soft remove refuses
/// (dirty tree, untracked deps). Errors only when even `--force` fails.
fn remove_worktree(main: &Path, wt: &Path) -> Result<()> {
    let soft = Command::new("git")
        .arg("-C")
        .arg(main)
        .args(["worktree", "remove"])
        .arg(wt)
        .output()?;
    if soft.status.success() {
        return Ok(());
    }
    let hard = Command::new("git")
        .arg("-C")
        .arg(main)
        .args(["worktree", "remove", "--force"])
        .arg(wt)
        .output()?;
    if hard.status.success() {
        return Ok(());
    }
    anyhow::bail!(
        "git worktree remove: {}",
        String::from_utf8_lossy(&hard.stderr).trim()
    )
}

fn delete_branch(main: &Path, branch: &str) {
    let _ = Command::new("git")
        .arg("-C")
        .arg(main)
        .args(["branch", "-D", branch])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}
