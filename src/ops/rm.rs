use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::config;
use crate::git;
use crate::workspace;

pub fn run(branch: &str, force: bool) -> Result<()> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;

    // Enumerate from git — the shared `workspace::worktrees` view `ws list` and
    // `ws clean` use — rather than scanning the task dir. A directory under
    // `.worktrees/<task>/` need not be a worktree at all: it can be a hand-made
    // copy or the residue of an interrupted removal. Teardown used to run the
    // whole lifecycle inside one of those and then fail `git worktree remove` on
    // it (there is no `<root>/<name>` main checkout to run git from), which
    // aborted the loop part-way — some repos torn down, the rest left behind —
    // and made the branch permanently unremovable until the stray dir was
    // deleted by hand.
    //
    // A registration whose directory is gone is excluded too: `git worktree
    // remove` fails on those, and before this enumeration change they were
    // simply invisible here (that's `git worktree prune`'s job, not `ws rm`'s).
    let known = workspace::worktrees(&ws.root, &cfg);
    let task = workspace::sanitize_branch(branch);
    let worktrees: Vec<(String, PathBuf)> = known
        .iter()
        .filter(|w| w.task.as_deref() == Some(task.as_str()) && w.path.is_dir())
        .map(|w| (w.repo.clone(), w.path.clone()))
        .collect();

    // The unregistered dirs in *this* task's dir. Reported, never acted on —
    // the same contract `ws clean` states for them.
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
    git::stdout_lines(git::git(wt).args(["log", "@{u}..HEAD", "--oneline"]))
}
