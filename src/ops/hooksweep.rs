//! Shared driver for `ws up` and `ws down` — the symmetric hook-runner pair.
//!
//! Both ops do the same thing: for every worktree of a branch, run one hook
//! event (`up` or `down`), isolating per-repo failures so one repo's broken
//! hook doesn't strand the others. They differ only in the event name, header,
//! and closing summary — those stay in the op modules.

use anyhow::Result;

use crate::config::{self, WsConfig};
use crate::hooks;
use crate::workspace;

/// Outcome of a sweep, for the caller to render its own summary from.
pub struct SweepResult {
    /// Repos whose hook ran and exited 0.
    pub ran: Vec<String>,
    /// Repos that had a worktree but no hook for this event (skipped, dim line).
    pub skipped: Vec<String>,
    /// Repos whose hook exited non-zero. Non-empty ⇒ the op should exit 1.
    pub failed: Vec<String>,
}

/// Run `event`'s hook across every worktree of `branch`. Prints the op header
/// (`◐ ws-<op> <branch>`), a step line per repo, and streams hook output. A
/// missing hook is a dim skip; a failing hook is reported but does not stop the
/// remaining repos.
///
/// Hard-fails with a preflight error (exit 2) when the branch has no worktrees.
pub fn sweep(branch: &str, event: &str, op: &str) -> Result<SweepResult> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;
    let sanitized = workspace::sanitize_branch(branch);

    // Iterate repos in config order (deterministic output), keeping only those
    // that actually have a worktree for this branch. Repos not part of the task
    // are silently absent — they were never brought up.
    let targets: Vec<(String, std::path::PathBuf)> = cfg
        .repos
        .iter()
        .map(|r| r.name())
        .filter_map(|name| {
            workspace::worktree_for(&ws.root, branch, name).map(|wt| (name.to_string(), wt))
        })
        .collect();

    if targets.is_empty() {
        return Err(super::preflight(format!(
            "no worktrees for {branch}; run `ws new` first"
        )));
    }

    crate::ui::header(op, branch);

    let mut result = SweepResult { ran: Vec::new(), skipped: Vec::new(), failed: Vec::new() };

    for (repo, wt) in &targets {
        super::bail_if_interrupted()?;

        let hook_override = repo_hook_override(&cfg, repo, event);
        let Some(rel) = hooks::lookup(&ws.root, repo, event, hook_override) else {
            crate::ui::skip(repo, &format!("no {event} hook"));
            result.skipped.push(repo.clone());
            continue;
        };

        let step = crate::ui::step(repo, &format!("{event} hook"));
        match hooks::run(&ws.root, &rel, wt, repo, &sanitized, event) {
            Ok(()) => {
                step.ok();
                result.ran.push(repo.clone());
            }
            Err(e) => {
                step.fail(&format!("{e}"));
                result.failed.push(repo.clone());
            }
        }
    }

    Ok(result)
}

fn repo_hook_override<'a>(cfg: &'a WsConfig, repo: &str, event: &str) -> Option<&'a str> {
    cfg.repos
        .iter()
        .find(|r| r.name() == repo)
        .and_then(|r| r.hook_override(event))
}
