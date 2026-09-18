use anyhow::Result;
use std::collections::BTreeMap;
use std::thread;
use std::time::Duration;

use crate::config;
use crate::git;
use crate::hooks;
use crate::inject;
use crate::ui::{ListGroup, ListRow};
use crate::workspace;

/// Slower `status` hooks are reported as "status unknown".
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);

/// The per-worktree work of `run`: git dirtiness, host-process health, declared status probe. Returned as a struct so the caller can rejoin rows in the same order it enumerated worktrees, and replay `ui::note`s serially — a fan-out that prints as it computes would let notes and the header interleave.
struct RowJob {
    task_key: String,
    row: ListRow,
    branch: Option<String>,
    notes: Vec<String>,
}

pub fn run() -> Result<()> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;
    // `None` = unreachable remote; the listing still renders, just without status.
    let site = crate::remote::Site::resolve(&cfg).ok();

    crate::ui::header("ws-list", "");

    // Shared with `remote set` and `clean` so the ops can never disagree.
    let worktrees = workspace::worktrees(&ws.root, &cfg);

    // Fan out — each row is one `git status` and up to two shell/ssh probes; on a 15-stack workspace the serial version is bound by the slowest probe times the count. `thread::scope` borrows `&cfg`/`&site`/`&ws.root` without needing `'static` clones and joins on drop, so a panicking probe surfaces as a panic here rather than a silent zombie.
    let jobs: Vec<RowJob> = thread::scope(|s| {
        let handles: Vec<_> = worktrees
            .into_iter()
            .map(|wt| s.spawn(|| compute_row(&cfg, site.as_ref(), &ws.root, wt)))
            .collect();
        handles.into_iter().map(|h| h.join().expect("row worker panicked")).collect()
    });

    let mut tasks: BTreeMap<String, Vec<(ListRow, Option<String>)>> = BTreeMap::new();
    for job in jobs {
        for note in job.notes {
            crate::ui::note(&note);
        }
        tasks.entry(job.task_key).or_default().push((job.row, job.branch));
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

/// Extracted so `thread::scope` can fan out one thread per worktree; the original inline loop body verbatim, minus the direct `ui::note` call (deferred through `RowJob::notes` so parallel workers can't interleave into the listing).
fn compute_row(
    cfg: &config::WsConfig,
    site: Option<&crate::remote::Site>,
    ws_root: &std::path::Path,
    wt: workspace::Worktree,
) -> RowJob {
    let workspace::Worktree { repo, path: wt_path, branch, task } = wt;
    let repo = repo.as_str();
    let managed = task.is_some();
    let task_key = task.clone().unwrap_or_else(|| "(unmanaged)".to_string());
    let dirty = git::is_dirty(&wt_path);
    let sanitized = workspace::sanitize_branch(branch.as_deref().unwrap_or(""));
    let mut notes: Vec<String> = Vec::new();

    // Only ws can report a crashed host process; a `status` script assumes the server is running.
    let host_health = (managed && cfg.is_host_repo(repo))
        .then(|| {
            let site = site?;
            let name = crate::supervise::proc_name(branch.as_deref().unwrap_or(""), repo);
            let ports = host_probe_ports(cfg, site, ws_root, &wt_path, &sanitized, repo);
            crate::supervise::health(site, &name, &ports)
        })
        .flatten();

    // Lazy — skipped when ws already has bad news about a host process.
    let mut declared = || if !managed {
        None
    } else if let Some(r) = cfg.remote.as_ref() {
        // Remote workspace: run the declared `status` script ON THE SERVER; skip when unreachable — remote status isn't knowable from here.
        match site {
            Some(s @ crate::remote::Site::Remote { .. }) => {
                cfg.command_for(repo, "status", cfg.is_container_repo(repo, &wt_path)).and_then(|cmd| {
                    let extra_env = inject::prepare(
                        cfg, s, ws_root, &wt_path, &sanitized, repo, inject::Phase::Inspect,
                    )
                    .unwrap_or_default();
                    let base = s.abs_base(&r.worktree_base(ws_root));
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
            cfg, ws_root, repo, "status", cfg.is_container_repo(repo, &wt_path),
        ) {
            Ok(cmd) => cmd?,
            Err(e) => {
                notes.push(format!("{repo}: {e}"));
                return Some("hook file not migrated".to_string());
            }
        };
        let extra_env = inject::prepare(
            cfg, &crate::remote::Site::Local, ws_root, &wt_path, &sanitized, repo,
            inject::Phase::Inspect,
        )
        .unwrap_or_default();
        hooks::capture_command(
            ws_root, &wt_path, repo, &sanitized, "status", &cmd, STATUS_TIMEOUT, &extra_env,
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
        crate::ports::load(ws_root, &sanitized, repo).into_iter().collect()
    } else {
        Vec::new()
    };

    let row = ListRow { repo: repo.to_string(), dirty, status, note, services };
    RowJob { task_key, row, branch, notes }
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
            root.join(".ws-meta/6_feat-x-3_api.json"),
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
