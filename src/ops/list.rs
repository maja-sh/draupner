use anyhow::Result;
use std::collections::BTreeMap;
use std::thread;
use std::time::Duration;

use crate::config;
use crate::git;
use crate::hooks;
use crate::inject;
use crate::ui::{classify_status, ListGroup, ListRow, ListShortRow, StatusKind};
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

pub fn run(short: bool) -> Result<()> {
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

    if short {
        // `(unmanaged)` never has status by definition — hide it from the triage view.
        let rows: Vec<ListShortRow> = tasks
            .iter()
            .filter(|(key, _)| key.as_str() != "(unmanaged)")
            .map(|(key, rows)| aggregate_short_row(key, rows))
            .collect();
        crate::ui::list_short(&rows);
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

/// Roll a task's repo rows up to `(worst-of glyph, running/total label)`. Any crashed repo dominates; a mix of healthy and stopped reads as degraded, not stopped, because "some up" is different from "none up". A row whose status ws couldn't classify counts against the numerator but not for the glyph — the visible ratio tells the truth about what ws knows.
fn aggregate_short_row(key: &str, rows: &[(ListRow, Option<String>)]) -> ListShortRow {
    let kinds: Vec<Option<StatusKind>> = rows
        .iter()
        .map(|(r, _)| r.status.as_deref().and_then(classify_status))
        .collect();
    let total = kinds.len();
    let healthy = kinds.iter().filter(|k| **k == Some(StatusKind::Healthy)).count();
    let crashed = kinds.iter().any(|k| *k == Some(StatusKind::Crashed));
    let stopped = kinds.iter().all(|k| *k == Some(StatusKind::Stopped));
    let all_healthy = kinds.iter().all(|k| *k == Some(StatusKind::Healthy));

    let (kind, label) = if crashed {
        (Some(StatusKind::Crashed), "crashed".to_string())
    } else if all_healthy {
        (Some(StatusKind::Healthy), format!("running {healthy}/{total}"))
    } else if stopped {
        (Some(StatusKind::Stopped), "stopped".to_string())
    } else if kinds.iter().any(|k| k.is_some()) {
        (Some(StatusKind::Degraded), format!("running {healthy}/{total}"))
    } else {
        (None, "unknown".to_string())
    };

    ListShortRow { task: group_label(key, rows), kind, label }
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

    let status = if managed {
        let (s, mut extra) = probe_status(
            cfg, site, ws_root, &wt_path, branch.as_deref().unwrap_or(""), &sanitized, repo,
        );
        notes.append(&mut extra);
        s
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

    // From `.ws-meta` only — no probing; reflects the last `up`.
    let services: Vec<(String, u16)> = if managed {
        crate::ports::load(ws_root, &sanitized, repo).into_iter().collect()
    } else {
        Vec::new()
    };

    let row = ListRow { repo: repo.to_string(), dirty, status, note, services };
    RowJob { task_key, row, branch, notes }
}

/// Classify a task-managed worktree's stack: host-supervised health first (ws
/// alone can report a crashed process), otherwise the declared `status` script
/// (locally, or over SSH for a remote workspace). Diagnostic notes are returned
/// separately so a parallel caller can serialize them without interleaving into
/// the listing.
///
/// **Precondition:** `wt_path` names a task-managed worktree (`.worktrees/<task>/<repo>`)
/// on this workspace's `branch`. Unmanaged worktrees and out-of-tree checkouts
/// have no ws lifecycle — the supervise/inject probes would run against paths
/// they were never meant to know about. `compute_row` and `add::require_task_stopped`
/// both filter to managed worktrees before calling this; new callers must too.
pub(super) fn probe_status(
    cfg: &config::WsConfig,
    site: Option<&crate::remote::Site>,
    ws_root: &std::path::Path,
    wt_path: &std::path::Path,
    branch: &str,
    sanitized: &str,
    repo: &str,
) -> (Option<String>, Vec<String>) {
    let mut notes: Vec<String> = Vec::new();

    // Only ws can report a crashed host process; a `status` script assumes the server is running.
    let host_health = cfg
        .is_host_repo(repo)
        .then(|| {
            let site = site?;
            let name = crate::supervise::proc_name(branch, repo);
            let ports = host_probe_ports(cfg, site, ws_root, wt_path, sanitized, repo);
            crate::supervise::health(site, &name, &ports)
        })
        .flatten();

    // Lazy — skipped when ws already has bad news about a host process.
    let mut declared = || -> Option<String> {
        if let Some(r) = cfg.remote.as_ref() {
            // Remote workspace: run the declared `status` script ON THE SERVER; skip when unreachable — remote status isn't knowable from here.
            match site {
                Some(s @ crate::remote::Site::Remote { .. }) => cfg
                    .command_for(repo, "status", cfg.is_container_repo(repo, wt_path))
                    .and_then(|cmd| {
                        let extra_env = inject::prepare(
                            cfg, s, ws_root, wt_path, sanitized, repo, inject::Phase::Inspect,
                        )
                        .unwrap_or_default();
                        let base = s.abs_base(&r.worktree_base(ws_root));
                        crate::remote::capture_status(
                            &r.connection, &base, sanitized, repo, &extra_env, &cmd,
                            STATUS_TIMEOUT.as_secs(),
                        )
                    }),
                _ => None,
            }
        } else {
            // Local status; Inspect phase — no allocation or derivation from a status probe. `ws list` is read-only, so a repo still on the removed hook-file mechanism gets the migration text as a note plus a short status label, never a silent blank or an aborted listing.
            let cmd = match hooks::resolve(
                cfg, ws_root, repo, "status", cfg.is_container_repo(repo, wt_path),
            ) {
                // `status` can never be interactive (refused at config load); .cmd only.
                Ok(script) => script?.cmd,
                Err(e) => {
                    notes.push(format!("{repo}: {e}"));
                    return Some("hook file not migrated".to_string());
                }
            };
            let extra_env = inject::prepare(
                cfg, &crate::remote::Site::Local, ws_root, wt_path, sanitized, repo,
                inject::Phase::Inspect,
            )
            .unwrap_or_default();
            hooks::capture_command(
                ws_root, wt_path, repo, sanitized, "status", &cmd, STATUS_TIMEOUT, &extra_env,
            )
        }
    };

    // ws's crash beats the script; a healthy process defers to the script for detail.
    let status = match host_health {
        Some(h) if !h.is_ok() => Some(h.label()),
        Some(h) => declared().or_else(|| Some(h.label())),
        None => declared(),
    };

    (status, notes)
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

    fn row(repo: &str, status: Option<&str>) -> (ListRow, Option<String>) {
        (
            ListRow {
                repo: repo.to_string(),
                dirty: false,
                status: status.map(|s| s.to_string()),
                note: None,
                services: Vec::new(),
            },
            Some("feat/x".to_string()),
        )
    }

    #[test]
    fn aggregate_reports_running_ratio_when_every_repo_is_healthy() {
        let rows = vec![row("api", Some("running 8/8")), row("app", Some("running :20032"))];
        let out = aggregate_short_row("feat/x", &rows);
        assert_eq!(out.kind, Some(StatusKind::Healthy));
        assert_eq!(out.label, "running 2/2");
    }

    #[test]
    fn aggregate_calls_a_crashed_repo_the_whole_tasks_state() {
        // Even one dead repo overshadows healthy peers: the triage view's job is to name the fire.
        let rows = vec![row("api", Some("running 8/8")), row("app", Some("crashed"))];
        let out = aggregate_short_row("feat/x", &rows);
        assert_eq!(out.kind, Some(StatusKind::Crashed));
        assert_eq!(out.label, "crashed");
    }

    #[test]
    fn aggregate_reads_all_stopped_as_stopped_not_partial() {
        let rows = vec![row("api", Some("stopped")), row("app", Some("stopped"))];
        let out = aggregate_short_row("feat/x", &rows);
        assert_eq!(out.kind, Some(StatusKind::Stopped));
        assert_eq!(out.label, "stopped");
    }

    #[test]
    fn aggregate_reads_mixed_healthy_and_stopped_as_degraded() {
        // "some up" and "none up" are different situations — a mixed task can't collapse to "stopped".
        let rows = vec![row("api", Some("running 8/8")), row("app", Some("stopped"))];
        let out = aggregate_short_row("feat/x", &rows);
        assert_eq!(out.kind, Some(StatusKind::Degraded));
        assert_eq!(out.label, "running 1/2");
    }

    #[test]
    fn aggregate_treats_a_container_partial_as_degraded() {
        let rows = vec![row("api", Some("running 5/8"))];
        let out = aggregate_short_row("feat/x", &rows);
        assert_eq!(out.kind, Some(StatusKind::Degraded));
        // The repo isn't `Healthy`, so it doesn't count toward the numerator.
        assert_eq!(out.label, "running 0/1");
    }

    #[test]
    fn aggregate_falls_to_none_when_no_repo_reported_a_known_state() {
        let rows = vec![row("api", None), row("app", Some("hook file not migrated"))];
        let out = aggregate_short_row("feat/x", &rows);
        assert_eq!(out.kind, None, "no known state means no glyph");
        assert_eq!(out.label, "unknown");
    }
}
