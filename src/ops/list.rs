use anyhow::Result;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::config;
use crate::git;
use crate::hooks;
use crate::inject;
use crate::ui::{ListGroup, ListRow};
use crate::workspace;

/// Wall-clock cap on the read-only `status` hook, per spec-2. A slower hook is
/// reported as "status unknown" (rendered as the missing-value dash).
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);

pub fn run() -> Result<()> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;

    crate::ui::header("ws-list", "");

    let canonical_root = ws.root.canonicalize().unwrap_or_else(|_| ws.root.clone());
    let worktrees_root = canonical_root.join(".worktrees");

    // Collected rows keyed by task dir name (managed) or "(unmanaged)".
    // Value carries each row plus its git branch so the group can label itself.
    let mut tasks: BTreeMap<String, Vec<(ListRow, Option<String>)>> = BTreeMap::new();

    for repo_entry in &cfg.repos {
        let repo = repo_entry.name();
        let repo_path = ws.root.join(repo);
        if !workspace::is_git_repo(&repo_path) {
            continue;
        }
        let repo_canon = repo_path.canonicalize().unwrap_or_else(|_| repo_path.clone());

        for (wt_path, branch) in git_worktrees(&repo_path) {
            let wt_canon = wt_path.canonicalize().unwrap_or_else(|_| wt_path.clone());
            // Skip the main checkout — it's not a task worktree.
            if wt_canon == repo_canon {
                continue;
            }

            let (task_key, managed) = classify(&wt_canon, &worktrees_root);
            let dirty = git::is_dirty(&wt_path);

            let status = if managed {
                hooks::resolve(&cfg, &ws.root, repo, "status").and_then(|action| {
                    let sanitized = workspace::sanitize_branch(branch.as_deref().unwrap_or(""));
                    let extra_env =
                        inject::prepare(&cfg, &ws.root, &wt_path, &sanitized, repo, inject::Phase::Inspect)
                            .unwrap_or_default();
                    hooks::capture_action(
                        &ws.root, &wt_path, repo, &sanitized, "status", &action, STATUS_TIMEOUT,
                        &extra_env,
                    )
                })
            } else {
                None
            };

            let note = if managed {
                branch.is_none().then(|| "detached".to_string())
            } else {
                Some(match &branch {
                    None => "unmanaged, detached".to_string(),
                    Some(b) => format!("unmanaged, {b}"),
                })
            };

            let row = ListRow { repo: repo.to_string(), dirty, status, note };
            tasks.entry(task_key).or_default().push((row, branch));
        }
    }

    if tasks.is_empty() {
        crate::ui::list_empty();
        return Ok(());
    }

    let groups: Vec<ListGroup> = tasks
        .into_iter()
        .map(|(key, rows)| {
            let label = group_label(&key, &rows);
            ListGroup { task: label, rows: rows.into_iter().map(|(r, _)| r).collect() }
        })
        .collect();

    crate::ui::list_table(&groups);
    Ok(())
}

/// Pick a group's display label. When every row shares the same branch, show
/// the (unsanitized) branch name; otherwise fall back to the task-dir key.
fn group_label(key: &str, rows: &[(ListRow, Option<String>)]) -> String {
    let mut branches = rows.iter().filter_map(|(_, b)| b.as_deref());
    if let Some(first) = branches.next() {
        if rows.iter().filter_map(|(_, b)| b.as_deref()).all(|b| b == first) {
            return first.to_string();
        }
    }
    key.to_string()
}

/// Classify a worktree path. Returns `(task_key, managed)`. Managed worktrees
/// live under `<root>/.worktrees/<task>/…`; anything else is "(unmanaged)".
fn classify(wt_canon: &Path, worktrees_root: &Path) -> (String, bool) {
    match workspace::task_repo_under(wt_canon, worktrees_root) {
        Some((task, _)) => (task, true),
        None => ("(unmanaged)".to_string(), false),
    }
}

/// Parse `git worktree list --porcelain` into `(path, branch)` pairs. `branch`
/// is `None` for a detached HEAD. The main checkout is included by git and
/// filtered out by the caller.
fn git_worktrees(repo: &Path) -> Vec<(PathBuf, Option<String>)> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "list", "--porcelain"])
        .output();
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&out.stdout);

    let mut result = Vec::new();
    let mut cur_path: Option<PathBuf> = None;
    let mut cur_branch: Option<String> = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            // A new record begins — flush the previous one.
            if let Some(path) = cur_path.take() {
                result.push((path, cur_branch.take()));
            }
            cur_path = Some(PathBuf::from(p));
            cur_branch = None;
        } else if let Some(b) = line.strip_prefix("branch ") {
            cur_branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
        }
        // "detached", "bare", "HEAD …", "locked", "prunable" need no handling.
    }
    if let Some(path) = cur_path.take() {
        result.push((path, cur_branch.take()));
    }
    result
}
