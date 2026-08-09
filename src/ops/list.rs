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
                let ports = host_probe_ports(&cfg, site, &ws.root, &wt_path, &sanitized, repo);
                crate::supervise::health(site, &name, &ports)
            })
            .flatten();

        // Lazy — skipped when ws already has bad news about a host process.
        let declared = || if !managed {
            None
        } else if let Some(r) = cfg.remote.as_ref() {
            // Remote workspace: run the declared `status` script ON THE SERVER; skip when unreachable — remote status isn't knowable from here.
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
            // Local status; Inspect phase — no allocation or derivation from a status probe. `ws list` is read-only, so a repo still on the removed hook-file mechanism gets the migration text as a note plus a short status label, never a silent blank or an aborted listing.
            let cmd = match hooks::resolve(
                &cfg, &ws.root, repo, "status", cfg.is_container_repo(repo, &wt_path),
            ) {
                Ok(cmd) => cmd?,
                Err(e) => {
                    crate::ui::note(&format!("{repo}: {e}"));
                    return Some("hook file not migrated".to_string());
                }
            };
            let extra_env = inject::prepare(
                &cfg, &crate::remote::Site::Local, &ws.root, &wt_path, &sanitized, repo,
                inject::Phase::Inspect,
            )
            .unwrap_or_default();
            hooks::capture_command(
                &ws.root, &wt_path, repo, &sanitized, "status", &cmd, STATUS_TIMEOUT, &extra_env,
            )
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

/// Not the raw `.ws-meta` port map — `ports::resolve` only ever adds keys, so a service removed from `.ws.json` leaves its allocation behind and probing it reported the process as squatting a stale port it never claimed.
fn host_probe_ports(
    cfg: &config::WsConfig,
    site: &crate::remote::Site,
    root: &std::path::Path,
    worktree: &std::path::Path,
    sanitized: &str,
    repo: &str,
) -> Vec<u16> {
    inject::prepare(cfg, site, root, worktree, sanitized, repo, inject::Phase::Inspect)
        .unwrap_or_default()
        .into_iter()
        .filter(|(k, _)| k.starts_with("WS_PORT_"))
        .filter_map(|(_, v)| v.parse().ok())
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The `.ws-meta` state a removed service leaves behind: its port is still in
    /// `ports` (nothing prunes that map short of teardown) while `services` was
    /// rewritten to the current set by the last `up`.
    #[test]
    fn host_probe_ports_ignores_a_port_left_by_a_removed_service() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join(".ws.json"),
            r#"{"repos":[{"name":"api","kind":"host","services":{"web":{"port":8080}}}]}"#,
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".ws-meta")).unwrap();
        std::fs::write(
            root.join(".ws-meta/feat-x-api.json"),
            r#"{"ports":{"gone":20001,"web":20002},"services":{"web":{"port":8080}}}"#,
        )
        .unwrap();

        let cfg = config::load(root).unwrap();
        let wt = root.join(".worktrees/feat-x/api");
        let ports = host_probe_ports(&cfg, &crate::remote::Site::Local, root, &wt, "feat-x", "api");
        assert_eq!(
            ports,
            vec![20002],
            "only the current service set's ports may be probed — :20001 belongs to a \
             service that no longer exists"
        );
    }
}
