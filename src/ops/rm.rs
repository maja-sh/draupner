use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config;
use crate::git;
use crate::workspace;

pub fn run(branch: &str, force: bool, keep_images: bool, keep_volumes: bool) -> Result<()> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;

    // Git-sourced so `rm` reaches stale registrations no filesystem scan would find. Match branch OR task-dir name so detached-HEAD worktrees still resolve.
    let known = workspace::worktrees(&ws.root, &cfg);
    let task = workspace::sanitize_branch(branch);
    let worktrees: Vec<(String, PathBuf)> = known
        .iter()
        .filter(|w| w.branch.as_deref() == Some(branch) || w.task.as_deref() == Some(task.as_str()))
        .map(|w| (w.repo.clone(), w.path.clone()))
        .collect();

    // Reported, not acted on — same contract `drup clean` states.
    let task_dir = workspace::task_dir(&ws.root, branch);
    let leftovers: Vec<String> = workspace::orphan_dirs(&ws.root, &known)
        .into_iter()
        .filter(|p| p.starts_with(&task_dir))
        .map(|p| p.display().to_string())
        .collect();

    if worktrees.is_empty() {
        let mut msg = format!("no worktrees for {branch}; nothing to remove");
        if !leftovers.is_empty() {
            msg.push_str(&format!(
                " ({} director{} under {} {} not registered with git — `drup clean` lists them)",
                leftovers.len(),
                if leftovers.len() == 1 { "y" } else { "ies" },
                task_dir.display(),
                if leftovers.len() == 1 { "is" } else { "are" },
            ));
        }
        return Err(super::preflight(msg));
    }

    crate::ui::header("drup-rm", branch);

    if !leftovers.is_empty() {
        crate::ui::note(&format!(
            "{} director{} under {} {} not registered with git — left in place; \
             remove by hand once you've checked {}:",
            leftovers.len(),
            if leftovers.len() == 1 { "y" } else { "ies" },
            task_dir.display(),
            if leftovers.len() == 1 { "is" } else { "are" },
            if leftovers.len() == 1 { "it" } else { "them" },
        ));
        crate::ui::command_block(&leftovers);
    }

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
            "worktree(s) not clean; re-run `drup rm {branch} --force` to remove anyway"
        )));
    }

    let hook_failed =
        super::teardown::remove_task(&ws.root, &cfg, branch, &worktrees, keep_images, keep_volumes)?;

    if hook_failed {
        return Err(super::op_failure(format!(
            "removed worktrees for {branch}, but one or more hooks failed (see above)"
        )));
    }

    crate::ui::summary_ok(&[format!("Removed worktrees for {branch}")]);
    crate::ui::nudge(&format!("drup new <repos> {branch} to recreate"));
    Ok(())
}

/// Commits present locally but not on the tracked upstream; empty when there is no upstream or on error.
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
