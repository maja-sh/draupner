use anyhow::Result;
use std::collections::BTreeMap;
use std::time::Duration;

use crate::config;
use crate::git;
use crate::inject;
use crate::ui::{ListGroup, ListRow};
use crate::workspace;

/// A `status` hook slower than this is reported as "status unknown" rather than
/// failing the listing.
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);

pub fn run() -> Result<()> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;
    // Resolved once — one SSH round trip for a remote workspace. `None` means a
    // remote we can't reach, which omits status rather than failing the listing.
    let site = crate::remote::Site::resolve(&cfg).ok();

    crate::ui::header("ws-list", "");

    let mut tasks: BTreeMap<String, Vec<(ListRow, Option<String>)>> = BTreeMap::new();

    // The same enumeration `remote set` and `clean` use, so the ops can never
    // disagree about which worktrees exist.
    for wt in workspace::worktrees(&ws.root, &cfg) {
        let workspace::Worktree { repo, path: wt_path, branch, task } = wt;
        let repo = repo.as_str();
        let managed = task.is_some();
        let task_key = task.unwrap_or_else(|| "(unmanaged)".to_string());
        let dirty = git::is_dirty(&wt_path);

        let sanitized = workspace::sanitize_branch(branch.as_deref().unwrap_or(""));

        // Only ws's own supervision can say "this crashed": a `status` script
        // describes a server it assumes is running, and reports nothing at all
        // once the process is gone.
        let host_health = (managed && cfg.is_host_repo(repo))
            .then(|| {
                let site = site.as_ref()?;
                let name = crate::supervise::proc_name(branch.as_deref().unwrap_or(""), repo);
                let ports = host_probe_ports(&cfg, site, &ws.root, &wt_path, &sanitized, repo);
                crate::supervise::health(site, &name, &ports)
            })
            .flatten();

        // A closure, not a value: when ws already has bad news about a host
        // process, that answer wins and this never runs.
        let declared = || {
            if !managed {
                return None;
            }
            let site = site.as_ref()?;
            let ctx = crate::lifecycle::Ctx {
                cfg: &cfg, site, root: &ws.root, worktree: &wt_path,
                branch: branch.as_deref().unwrap_or(""), sanitized: &sanitized, repo,
            };
            // `ws list` must keep listing: a repo still on the removed hook-file
            // mechanism gets the migration text as a note and a status label,
            // never an aborted listing.
            let cmd = match ctx.command("status") {
                Ok(cmd) => cmd?,
                Err(e) => {
                    crate::ui::note(&format!("{repo}: {e}"));
                    return Some("hook file not migrated".to_string());
                }
            };
            // `Inspect` allocates nothing; a read-only op must not hand out ports.
            let extra_env = inject::prepare(
                &cfg, site, &ws.root, &wt_path, &sanitized, repo, inject::Phase::Inspect,
            )
            .unwrap_or_default();
            ctx.capture("status", &cmd, &extra_env, STATUS_TIMEOUT)
        };

        // ws's own record wins only when it has something wrong to report; a
        // healthy process still lets a declared script say more than "running".
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

        let row = ListRow { repo: repo.to_string(), dirty, status, note };
        tasks.entry(task_key).or_default().push((row, branch));
    }

    if tasks.is_empty() {
        crate::ui::empty("no worktrees");
        return Ok(());
    }

    let groups: Vec<ListGroup> = tasks
        .into_iter()
        .map(|(key, rows)| {
            let label = group_label(&key, &rows);
            let path = if key == "(unmanaged)" {
                String::new()
            } else {
                workspace::task_dir(&ws.root, &key).display().to_string()
            };
            ListGroup { task: label, path, rows: rows.into_iter().map(|(r, _)| r).collect() }
        })
        .collect();

    crate::ui::list_table(&groups);
    Ok(())
}

/// The `WS_PORT_*` the current injection would hand the process — the same set
/// `ws up` probes.
///
/// Deliberately *not* the raw `.ws-meta` port map: that map is append-only, so a
/// service removed from `.ws.json` leaves its allocation behind forever and
/// probing the stale port reports a healthy process as down.
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

fn group_label(key: &str, rows: &[(ListRow, Option<String>)]) -> String {
    let mut branches = rows.iter().filter_map(|(_, b)| b.as_deref());
    if let Some(first) = branches.next() {
        if branches.all(|b| b == first) {
            return first.to_string();
        }
    }
    key.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixture is the state a removed service leaves behind: its port is
    /// still in `ports` while `services` was rewritten by the last `up`.
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
