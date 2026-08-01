use anyhow::Result;
use std::collections::BTreeMap;
use std::time::Duration;

use crate::config;
use crate::git;
use crate::inject;
use crate::ui::{ListGroup, ListRow};
use crate::workspace;

/// Wall-clock cap on the read-only `status` hook, per spec-2. A slower hook is
/// reported as "status unknown" (rendered as the missing-value dash).
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);

pub fn run() -> Result<()> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;
    // Where the stacks live. Resolved once (one SSH for a remote workspace). `None`
    // ⇒ a remote server we can't reach — status is then simply omitted (we can't
    // probe the remote stack; the rest of the listing still renders).
    let site = crate::remote::Site::resolve(&cfg).ok();

    crate::ui::header("ws-list", "");

    // Collected rows keyed by task dir name (managed) or "(unmanaged)".
    // Value carries each row plus its git branch so the group can label itself.
    let mut tasks: BTreeMap<String, Vec<(ListRow, Option<String>)>> = BTreeMap::new();

    // One shared enumeration — the same one `remote set` and `clean` use, so the
    // ops can never disagree about which worktrees exist.
    for wt in workspace::worktrees(&ws.root, &cfg) {
        let workspace::Worktree { repo, path: wt_path, branch, task } = wt;
        let repo = repo.as_str();
        let managed = task.is_some();
        let task_key = task.unwrap_or_else(|| "(unmanaged)".to_string());
        let dirty = git::is_dirty(&wt_path);

        let sanitized = workspace::sanitize_branch(branch.as_deref().unwrap_or(""));

        // A `kind: host` repo's process is supervised by ws itself, so ws can answer
        // for it without asking the repo — and it is the only one that *can* say
        // "this crashed": a `status` script describes a server it assumes is
        // running, and reports nothing at all once the process is gone. That state
        // was invisible until now.
        let host_health = (managed && cfg.is_host_repo(repo))
            .then(|| {
                let site = site.as_ref()?;
                let name = crate::supervise::proc_name(branch.as_deref().unwrap_or(""), repo);
                let ports = host_probe_ports(&cfg, site, &ws.root, &wt_path, &sanitized, repo);
                crate::supervise::health(site, &name, &ports)
            })
            .flatten();

        // The declared `status` script, run wherever the stack lives — locally, or
        // ON THE SERVER for a remote workspace, which the dispatcher decides.
        // Skipped when there's no site: that only happens for a remote server we
        // can't reach, and the status of a stack over there isn't knowable from
        // here. Lazy: when ws already has bad news about a host process, that's the
        // more important answer and this never runs.
        let declared = || {
            if !managed {
                return None;
            }
            let site = site.as_ref()?;
            let ctx = crate::lifecycle::Ctx {
                cfg: &cfg, site, root: &ws.root, worktree: &wt_path,
                branch: branch.as_deref().unwrap_or(""), sanitized: &sanitized, repo,
            };
            // `ws list` is read-only and must keep listing, so a repo still on the
            // removed hook-file mechanism gets the full migration text as a note
            // (the table renders after this loop) and a short status label —
            // never a silent blank, and never an aborted listing.
            let cmd = match ctx.command("status") {
                Ok(cmd) => cmd?,
                Err(e) => {
                    crate::ui::note(&format!("{repo}: {e}"));
                    return Some("hook file not migrated".to_string());
                }
            };
            // Inspect — no allocation/derive; the recorded ports come from `.ws-meta`.
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
        crate::ui::list_empty();
        return Ok(());
    }

    let groups: Vec<ListGroup> = tasks
        .into_iter()
        .map(|(key, rows)| {
            let label = group_label(&key, &rows);
            // The task dir holding this branch's worktrees, so a reader (or a
            // Claude session) can find the files. The unmanaged group has no
            // single dir, so it gets none.
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

/// The ports to hold a host process to: exactly the `WS_PORT_*` the current
/// injection would hand it, which is what `ws up` probes.
///
/// Deliberately *not* the raw `.ws-meta` port map: `ports::resolve` only ever
/// adds keys, so a service removed or renamed in `.ws.json` leaves its
/// allocation behind forever. Probing that stale port reported "running, nothing
/// on :<port>" — and, because a not-ok health short-circuits the declared
/// `status` script below, suppressed the repo's real status too — for a process
/// that was serving everything it had been asked to. `up` and `list` disagreed
/// about the same worktree.
///
/// `Phase::Inspect` allocates nothing and renders nothing; it reads back the
/// recorded service set (falling back to the declared one) and the ports already
/// held for it.
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

/// Pick a group's display label. When every row shares the same branch, show
/// the (unsanitized) branch name; otherwise fall back to the task-dir key.
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
