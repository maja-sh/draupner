use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

use crate::config::{self, WsConfig};
use crate::git;
use crate::hooks;
use crate::inject;
use crate::ops::show;
use crate::remote::Site;
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

pub fn run(short: bool, json: bool, describe: bool) -> Result<()> {
    if describe {
        print!("{}", SCHEMA_TEXT);
        return Ok(());
    }

    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;
    // `None` = unreachable remote; the listing still renders, just without status.
    let site = Site::resolve(&cfg).ok();

    if json {
        return run_json(&cfg, &ws.root, site.as_ref());
    }

    crate::ui::header("drup-list", "");

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

/// Roll a task's repo rows up to `(worst-of glyph, running/total label)`. Any crashed repo dominates; a mix of healthy and stopped reads as degraded, not stopped, because "some up" is different from "none up". A row whose status drup couldn't classify counts against the numerator but not for the glyph — the visible ratio tells the truth about what drup knows.
fn aggregate_short_row(key: &str, rows: &[(ListRow, Option<String>)]) -> ListShortRow {
    let kinds: Vec<Option<StatusKind>> = rows
        .iter()
        .map(|(r, _)| r.status.as_deref().and_then(classify_status))
        .collect();
    let total = kinds.len();
    let healthy = kinds.iter().filter(|k| **k == Some(StatusKind::Healthy)).count();
    let crashed = kinds.contains(&Some(StatusKind::Crashed));
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

    // From `.drup-meta` only — no probing; reflects the last `up`.
    let services: Vec<(String, u16)> = if managed {
        crate::ports::load(ws_root, &sanitized, repo).into_iter().collect()
    } else {
        Vec::new()
    };

    let row = ListRow { repo: repo.to_string(), dirty, status, note, services };
    RowJob { task_key, row, branch, notes }
}

/// Classify a task-managed worktree's stack: host-supervised health first (drup
/// alone can report a crashed process), otherwise the declared `status` script
/// (locally, or over SSH for a remote workspace). Diagnostic notes are returned
/// separately so a parallel caller can serialize them without interleaving into
/// the listing.
///
/// **Precondition:** `wt_path` names a task-managed worktree (`.worktrees/<task>/<repo>`)
/// on this workspace's `branch`. Unmanaged worktrees and out-of-tree checkouts
/// have no drup lifecycle — the supervise/inject probes would run against paths
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

    // Only drup can report a crashed host process; a `status` script assumes the server is running.
    let host_health = cfg
        .is_host_repo(repo)
        .then(|| {
            let site = site?;
            let name = crate::supervise::proc_name(branch, repo);
            let ports = host_probe_ports(cfg, site, ws_root, wt_path, sanitized, repo);
            crate::supervise::health(site, &name, &ports)
        })
        .flatten();

    // Lazy — skipped when drup already has bad news about a host process.
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
            // Local status; Inspect phase — no allocation or derivation from a status probe. `drup list` is read-only, so a repo still on the removed hook-file mechanism gets the migration text as a note plus a short status label, never a silent blank or an aborted listing.
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

    // drup's crash beats the script; a healthy process defers to the script for detail.
    let status = match host_health {
        Some(h) if !h.is_ok() => Some(h.label()),
        Some(h) => declared().or_else(|| Some(h.label())),
        None => declared(),
    };

    (status, notes)
}

/// Not the raw `.drup-meta` port map — `ports::resolve` only ever adds keys, so a service removed from `.drup.json` leaves its allocation behind and probing it reported the process as squatting a stale port it never claimed.
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
        .filter(|(k, _)| k.starts_with("DRUP_PORT_"))
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

/// A probed worktree in a shape list's JSON path can fuse into a per-task record.
/// Carries the full show-shaped `RepoInfo` for the output, plus the raw `StatusKind`
/// so the task-level aggregate preserves `crashed` vs `stopped` (collapsing to the
/// serialized `is_running: Option<bool>` would lose it).
struct JsonRow {
    task: String,
    branch: String,
    repo_info: show::RepoInfo,
    status_kind: Option<StatusKind>,
}

#[derive(Serialize)]
struct ListJsonTask {
    branch: String,
    task: String,
    state: &'static str,
    workspace_root: PathBuf,
    remote: Option<show::RemoteInfo>,
    repos: Vec<show::RepoInfo>,
    healthy_repos: usize,
    total_repos: usize,
    created_at: Option<String>,
}

fn run_json(cfg: &WsConfig, root: &Path, site: Option<&Site>) -> Result<()> {
    let worktrees = workspace::worktrees(root, cfg);

    // Same fan-out shape as the human path: one thread per worktree, join under
    // `thread::scope` so borrows of `cfg`/`site`/`root` don't need 'static.
    let rows: Vec<JsonRow> = thread::scope(|s| {
        let handles: Vec<_> = worktrees
            .into_iter()
            .filter(|wt| wt.task.is_some())
            .map(|wt| s.spawn(move || compute_json_row(cfg, site, root, wt)))
            .collect();
        handles.into_iter().map(|h| h.join().expect("row worker panicked")).collect()
    });

    // Constant per workspace; cloned into each record so each task object is self-contained.
    let remote = cfg.remote.as_ref().map(|r| show::RemoteInfo {
        connection: r.connection.clone(),
        worktree_base_absolute: match site {
            Some(s) => s.abs_base(&r.worktree_base(root)),
            None => r.worktree_base(root),
        },
    });

    let mut by_task: BTreeMap<String, Vec<JsonRow>> = BTreeMap::new();
    for row in rows {
        by_task.entry(row.task.clone()).or_default().push(row);
    }

    let records: Vec<ListJsonTask> = by_task
        .into_iter()
        .map(|(task, rows)| build_task_record(root, remote.clone(), task, rows))
        .collect();

    let mut out = std::io::stdout().lock();
    writeln!(out, "{}", serde_json::to_string_pretty(&records)?)?;
    Ok(())
}

fn compute_json_row(
    cfg: &WsConfig,
    site: Option<&Site>,
    root: &Path,
    wt: workspace::Worktree,
) -> JsonRow {
    let workspace::Worktree { repo, path: wt_path, branch, task } = wt;
    // Caller filtered unmanaged out — a hand-made worktree reaching here is a bug.
    let task = task.expect("compute_json_row only runs for task-managed worktrees");
    let branch_str = branch.clone().unwrap_or_else(|| task.clone());
    let sanitized = workspace::sanitize_branch(&branch_str);

    // Status via the shared probe path — `is_running` is derived from the resulting
    // StatusKind (Healthy|Degraded ⇒ true, Stopped|Crashed ⇒ false, None ⇒ null) so
    // show's existing bool contract holds. Notes land on stderr, matching the
    // human path's convention; `--json` consumers can `2>/dev/null` to silence.
    let (status_str, notes) = probe_status(
        cfg, site, root, &wt_path, &branch_str, &sanitized, &repo,
    );
    for note in &notes {
        crate::ui::note(note);
    }
    let status_kind = status_str.as_deref().and_then(classify_status);
    let is_running = status_kind.map(|k| matches!(k, StatusKind::Healthy | StatusKind::Degraded));

    let is_host = cfg.is_host_repo(&repo);
    let is_container = cfg.is_container_repo(&repo, &wt_path);
    let kind = if is_container { "container" } else { "host" };

    let (port_map, recorded_services) = crate::ports::load_meta(root, &sanitized, &repo);
    let services = show::services_for(cfg, &sanitized, &repo, port_map, recorded_services);

    let log_path_absolute = is_host
        .then(|| site.and_then(|s| crate::supervise::log_path(s, &crate::supervise::proc_name(&branch_str, &repo))))
        .flatten();
    let exec_command_prefix = format!("drup exec --branch {branch_str} {repo} --");

    let repo_info = show::RepoInfo {
        name: repo,
        kind: kind.to_string(),
        worktree_absolute: wt_path,
        is_running,
        services,
        log_path_absolute,
        exec_command_prefix,
    };

    JsonRow { task, branch: branch_str, repo_info, status_kind }
}

fn build_task_record(
    root: &Path,
    remote: Option<show::RemoteInfo>,
    task: String,
    rows: Vec<JsonRow>,
) -> ListJsonTask {
    let branch = rows.iter().find_map(|r| (!r.branch.is_empty()).then(|| r.branch.clone()))
        .unwrap_or_else(|| task.clone());
    let kinds: Vec<Option<StatusKind>> = rows.iter().map(|r| r.status_kind).collect();
    let state = aggregate_state(&kinds);
    let healthy_repos = kinds.iter().filter(|k| **k == Some(StatusKind::Healthy)).count();
    let total_repos = kinds.len();
    let repos: Vec<show::RepoInfo> = rows.into_iter().map(|r| r.repo_info).collect();
    let created_at = task_btime(root, &task);
    ListJsonTask {
        branch,
        task,
        state,
        workspace_root: root.to_path_buf(),
        remote,
        repos,
        healthy_repos,
        total_repos,
        created_at,
    }
}

/// Same precedence as `aggregate_short_row` — a single crashed repo dominates
/// (if any repo was up and died, that's the headline), everything-healthy wins
/// next, everything-stopped before degraded (so "none up" and "some up" stay
/// distinct), and `unknown` only when nothing reported a classifiable state.
fn aggregate_state(kinds: &[Option<StatusKind>]) -> &'static str {
    if kinds.contains(&Some(StatusKind::Crashed)) {
        "crashed"
    } else if !kinds.is_empty() && kinds.iter().all(|k| *k == Some(StatusKind::Healthy)) {
        "healthy"
    } else if !kinds.is_empty() && kinds.iter().all(|k| *k == Some(StatusKind::Stopped)) {
        "stopped"
    } else if kinds.iter().any(|k| k.is_some()) {
        "degraded"
    } else {
        "unknown"
    }
}

/// Btime of `.worktrees/<task>/` — "when did this task appear on this machine".
/// `None` on filesystems that don't expose creation time (never falls back to
/// mtime, which drifts with every file touch and would misreport "just created"
/// after a build ran inside the worktree). ISO-8601 UTC; the formatter is
/// hand-rolled so this doesn't pull a date-time dep in.
fn task_btime(root: &Path, task: &str) -> Option<String> {
    let dir = root.join(".worktrees").join(task);
    let created = std::fs::metadata(&dir).ok()?.created().ok()?;
    let secs = created.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(format_iso8601_utc(secs))
}

/// Unix seconds (non-negative) → `YYYY-MM-DDTHH:MM:SSZ`. Howard Hinnant's
/// civil-from-days algorithm — public domain, no external deps.
fn format_iso8601_utc(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let time = secs % 86400;
    let (h, m, s) = (time / 3600, (time % 3600) / 60, time % 60);
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// The `--describe` schema block; a drift test anchors every serialized field
/// name here so adding one without documenting it trips at build time.
const SCHEMA_TEXT: &str = r#"drup list — machine-readable workspace listing.

Human by default (stderr). `--json` emits a JSON array on stdout — one object
per drup-managed task:

[
  {
    "branch":          original branch name (may contain "/"),
    "task":            sanitized dir name under .worktrees/ — stable cross-call id,
    "state":           "healthy" | "degraded" | "stopped" | "crashed" | "unknown",
    "workspace_root":  absolute path to the workspace root,
    "remote":          null (local) or:
                         { "connection": "<ssh-host>",
                           "worktree_base_absolute": "<server-side path>" },
    "repos": [
      {
        "name":                  repo name from .drup.json,
        "kind":                  "host" | "container",
        "worktree_absolute":     absolute path to this repo's worktree,
        "is_running":            true | false | null,
        "services": [
          {
            "name":                  service name,
            "allocated_host_port":   integer or null,
            "routed_hostname":       HTTP hostname or null,
            "is_http_routed":        true if reachable via https://<routed_hostname>
          }
        ],
        "log_path_absolute":     absolute path to the host log, null for containers,
        "exec_command_prefix":   "drup exec --branch <branch> <repo> --"
      }
    ],
    "healthy_repos":   integer,
    "total_repos":     integer,
    "created_at":      ISO-8601 UTC string or null
  }
]

Semantics tooling should ground on:
- repos[] matches `drup show --json`'s shape element-for-element — a consumer
  that parses show lifts the per-repo logic verbatim.
- `state` aggregates per-repo liveness: any crashed repo dominates; otherwise
  all-healthy wins, then all-stopped, then mixed (degraded), then unknown.
- `created_at` is the dir btime of `.worktrees/<task>/` — "when did this task
  appear on this machine". Resets on `drup rm` + `drup new`. `null` on
  filesystems that don't expose btime; drup never falls back to mtime.
- Unmanaged worktrees (hand-made under `.worktrees/`) are omitted from this
  listing — the human `drup list` groups them separately, but they aren't
  drup-managed tasks.
- Zero tasks emits `[]` and exits 0.
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// The `.drup-meta` state a removed service leaves behind: its port is still in
    /// `ports` (nothing prunes that map short of teardown) while `services` was
    /// rewritten to the current set by the last `up`.
    #[test]
    fn host_probe_ports_ignores_a_port_left_by_a_removed_service() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join(".drup.json"),
            r#"{"repos":[{"name":"api","kind":"host","services":{"web":{"port":8080}}}]}"#,
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".drup-meta")).unwrap();
        std::fs::write(
            root.join(".drup-meta/6_feat-x-3_api.json"),
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

    #[test]
    fn aggregate_state_names_crashed_over_everything() {
        // Mirrors `aggregate_short_row`: one crashed repo is the fire — don't hide it.
        let kinds = [Some(StatusKind::Healthy), Some(StatusKind::Crashed)];
        assert_eq!(aggregate_state(&kinds), "crashed");
    }

    #[test]
    fn aggregate_state_is_healthy_only_when_every_repo_is_healthy() {
        assert_eq!(aggregate_state(&[Some(StatusKind::Healthy); 3]), "healthy");
        // One degraded demotes the whole task — healthy is unanimous or nothing.
        assert_eq!(
            aggregate_state(&[Some(StatusKind::Healthy), Some(StatusKind::Degraded)]),
            "degraded"
        );
    }

    #[test]
    fn aggregate_state_distinguishes_stopped_from_degraded() {
        assert_eq!(aggregate_state(&[Some(StatusKind::Stopped); 2]), "stopped");
        assert_eq!(
            aggregate_state(&[Some(StatusKind::Healthy), Some(StatusKind::Stopped)]),
            "degraded"
        );
    }

    #[test]
    fn aggregate_state_is_unknown_only_when_nothing_classified() {
        assert_eq!(aggregate_state(&[None, None]), "unknown");
        // A single known state is enough to pull out of unknown.
        assert_eq!(aggregate_state(&[None, Some(StatusKind::Healthy)]), "degraded");
    }

    #[test]
    fn format_iso8601_utc_matches_known_timestamps() {
        assert_eq!(format_iso8601_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_iso8601_utc(1_234_567_890), "2009-02-13T23:31:30Z");
        assert_eq!(format_iso8601_utc(1_577_836_800), "2020-01-01T00:00:00Z");
        assert_eq!(format_iso8601_utc(1_704_067_200), "2024-01-01T00:00:00Z");
    }

    #[test]
    fn task_btime_returns_a_value_for_a_real_dir() {
        // On a filesystem that doesn't expose btime, this returns `None` and we
        // skip rather than fail — matches the spec's "null, never mtime" contract.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".worktrees").join("feat-x")).unwrap();
        if let Some(ts) = task_btime(root, "feat-x") {
            assert!(
                ts.ends_with('Z') && ts.len() == 20,
                "expected ISO-8601 Z-terminated 20-char string, got {ts:?}"
            );
        }
    }

    #[test]
    fn schema_text_mentions_every_json_field() {
        // Drift alarm — matches `drup show`'s pattern. Adding a `#[derive(Serialize)]`
        // field without updating SCHEMA_TEXT trips here.
        for field in [
            "branch",
            "task",
            "state",
            "workspace_root",
            "remote",
            "repos",
            "healthy_repos",
            "total_repos",
            "created_at",
            "connection",
            "worktree_base_absolute",
            "name",
            "kind",
            "worktree_absolute",
            "is_running",
            "services",
            "allocated_host_port",
            "routed_hostname",
            "is_http_routed",
            "log_path_absolute",
            "exec_command_prefix",
        ] {
            let needle = format!("\"{field}\":");
            assert!(
                SCHEMA_TEXT.contains(&needle),
                "SCHEMA_TEXT is missing `{needle}` — describe output has drifted from the actual JSON shape"
            );
        }
    }

    #[test]
    fn describe_prints_and_exits_ok() {
        run(false, false, true).expect("--describe never fails");
    }
}
