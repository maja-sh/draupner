use anyhow::Result;
use std::collections::BTreeMap;
use std::time::Duration;

use crate::config;
use crate::git;
use crate::hooks;
use crate::inject;
use crate::ui::{ListGroup, ListRow};
use crate::workspace;

/// Slower `status` hooks are reported as "status unknown".
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);

pub fn run() -> Result<()> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;
    // `None` = unreachable remote; the listing still renders, just without status.
    let site = crate::remote::Site::resolve(&cfg).ok();

    crate::ui::header("ws-list", "");

    let mut tasks: BTreeMap<String, Vec<(ListRow, Option<String>)>> = BTreeMap::new();

    // Shared with `remote set` and `clean` so the ops can never disagree.
    for wt in workspace::worktrees(&ws.root, &cfg) {
        let workspace::Worktree { repo, path: wt_path, branch, task } = wt;
        let repo = repo.as_str();
        let managed = task.is_some();
        let task_key = task.unwrap_or_else(|| "(unmanaged)".to_string());
        let dirty = git::is_dirty(&wt_path);

        let sanitized = workspace::sanitize_branch(branch.as_deref().unwrap_or(""));

        // Only ws can report a crashed host process; a `status` script assumes the server is running.
        let host_health = (managed && cfg.is_host_repo(repo))
            .then(|| {
                let site = site.as_ref()?;
                let name = crate::supervise::proc_name(branch.as_deref().unwrap_or(""), repo);
                let ports: Vec<u16> =
                    crate::ports::load(&ws.root, &sanitized, repo).into_values().collect();
                crate::supervise::health(site, &name, &ports)
            })
            .flatten();

        // Lazy — skipped when ws already has bad news about a host process.
        let declared = || if !managed {
            None
        } else if let Some(r) = cfg.remote.as_ref() {
            // Runs on the server; a legacy `.ws-hooks` file on the host can't reach the remote stack.
            match site.as_ref() {
                Some(s @ crate::remote::Site::Remote { .. }) => {
                    cfg.command_for(repo, "status", cfg.is_container_repo(repo, &wt_path)).and_then(|cmd| {
                        let extra_env = inject::prepare(
                            &cfg, s, &ws.root, &wt_path, &sanitized, repo, inject::Phase::Inspect,
                        )
                        .unwrap_or_default();
                        let base = s.abs_base(&r.worktree_base(&ws.root));
                        crate::remote::capture_status(
                            &r.connection, &base, &sanitized, repo, &extra_env, &cmd,
                            STATUS_TIMEOUT.as_secs(),
                        )
                    })
                }
                _ => None,
            }
        } else {
            // Inspect phase must stay — no allocation or derivation from a status probe.
            hooks::resolve(&cfg, &ws.root, repo, "status", cfg.is_container_repo(repo, &wt_path)).and_then(|action| {
                let extra_env = inject::prepare(
                    &cfg, &crate::remote::Site::Local, &ws.root, &wt_path, &sanitized, repo,
                    inject::Phase::Inspect,
                )
                .unwrap_or_default();
                hooks::capture_action(
                    &ws.root, &wt_path, repo, &sanitized, "status", &action, STATUS_TIMEOUT,
                    &extra_env,
                )
            })
        };

        // ws's crash beats the script; a healthy process defers to the script for detail.
        let status = match host_health {
            Some(h) if !h.is_ok() => Some(h.label()),
            Some(h) => declared().or_else(|| Some(h.label())),
            None => declared(),
        };

        let note = if managed {
            branch.is_none().then(|| "detached".to_string())
        } else {
            Some(match &branch {
                None => "unmanaged, detached".to_string(),
                Some(b) => format!("unmanaged, {b}"),
            })
        };

        // From `.ws-meta` only — no probing; reflects the last `up`.
        let services: Vec<(String, u16)> = if managed {
            crate::ports::load(&ws.root, &sanitized, repo).into_iter().collect()
        } else {
            Vec::new()
        };

        let row = ListRow { repo: repo.to_string(), dirty, status, note, services };
        tasks.entry(task_key).or_default().push((row, branch));
    }

    if tasks.is_empty() {
        crate::ui::list_empty();
        return Ok(());
    }

    let groups: Vec<ListGroup> = tasks
        .into_iter()
        .map(|(key, rows)| {
            let label = group_label(&key, &rows);
            let path = if key == "(unmanaged)" {
                String::new()
            } else {
                ws.root.join(".worktrees").join(&key).display().to_string()
            };
            ListGroup { task: label, path, rows: rows.into_iter().map(|(r, _)| r).collect() }
        })
        .collect();

    crate::ui::list_table(&groups);
    Ok(())
}

/// Prefer the shared branch name; fall back to the task-dir key when rows disagree.
fn group_label(key: &str, rows: &[(ListRow, Option<String>)]) -> String {
    let mut branches = rows.iter().filter_map(|(_, b)| b.as_deref());
    if let Some(first) = branches.next() {
        if rows.iter().filter_map(|(_, b)| b.as_deref()).all(|b| b == first) {
            return first.to_string();
        }
    }
    key.to_string()
}
