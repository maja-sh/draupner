use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::config;
use crate::git;
use crate::workspace;

pub fn run(branch: &str, force: bool) -> Result<()> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;

    // Enumerate from git rather than scanning the task dir: a directory under
    // `.worktrees/<task>/` need not be a worktree at all, and a stale
    // registration has no directory for a filesystem scan to find — `rm` must
    // reach everything `ws list` shows. Matching the reported branch OR the
    // task dir name catches a detached worktree, which has no branch to match.
    let known = workspace::worktrees(&ws.root, &cfg);
    let task = workspace::sanitize_branch(branch);
    let worktrees: Vec<(String, PathBuf)> = known
        .iter()
        .filter(|w| w.branch.as_deref() == Some(branch) || w.task.as_deref() == Some(task.as_str()))
        .map(|w| (w.repo.clone(), w.path.clone()))
        .collect();

    // Unregistered dirs are reported, never acted on — the same contract
    // `ws clean` states for them.
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
                " ({} director{} under {} {} not registered with git — `ws clean` lists them)",
                leftovers.len(),
                if leftovers.len() == 1 { "y" } else { "ies" },
                task_dir.display(),
                if leftovers.len() == 1 { "is" } else { "are" },
            ));
        }
        return Err(super::preflight(msg));
    }

    crate::ui::header("ws-rm", branch);

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

    // The scan runs under `--force` too: the warnings are the record of what
    // was overridden.
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

/// Empty when no upstream is configured or the command fails, so a branch that
/// was never pushed anywhere does not block removal.
fn unpushed_commits(wt: &Path) -> Vec<String> {
    git::stdout_lines(git::git(wt).args(["log", "@{u}..HEAD", "--oneline"]))
}
