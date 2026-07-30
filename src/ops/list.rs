use anyhow::Result;
use std::collections::BTreeMap;
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
        let status = if !managed {
            None
        } else if let Some(r) = cfg.remote.as_ref() {
            // Remote workspace: run the declared `status` script ON THE SERVER
            // (a legacy `.ws-hooks` file lives on the host and can't). Skip when
            // the server is unreachable (`site` didn't resolve to `Remote`) —
            // status of the remote stack isn't knowable from here.
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
            // Local workspace: run the `status` hook/script locally (Inspect —
            // no allocation/derive; the recorded ports come from `.ws-meta`).
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
                ws.root.join(".worktrees").join(&key).display().to_string()
            };
            ListGroup { task: label, path, rows: rows.into_iter().map(|(r, _)| r).collect() }
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
