use anyhow::Result;
use std::path::Path;
use std::process::Command;

use crate::config;
use crate::git;
use crate::workspace;

pub fn run(branch: &str, force: bool) -> Result<()> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;

    // Git-sourced, so `rm` can reach everything `ws list` shows — a stale
    // registration has no directory for a filesystem scan to find.
    let worktrees = workspace::worktrees_for(&ws.root, &cfg, branch);

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
        let dirty = git::dirty_files(wt);
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

    let hook_failed = super::teardown::remove_task(&ws.root, &cfg, branch, &worktrees)?;

    if hook_failed {
        return Err(super::op_failure(format!(
            "removed worktrees for {branch}, but one or more hooks failed (see above)"
        )));
    }

    crate::ui::summary_ok(&[format!("Removed worktrees for {branch}")]);
    crate::ui::nudge(&format!("ws new <repos> {branch} to recreate"));
    Ok(())
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
