//! `ws remote set` / `ws remote unset` — flip a workspace between local and
//! remote **after** `ws init` (which refuses to re-run on an existing
//! `.ws.json`). The flip is workspace-level, and it's more than a config edit:
//! worktrees that predate the switch need their file sync backfilled (going
//! remote) or torn down (going local), else `ws up`/`exec` would target a
//! server that has no files (or mutagen would keep syncing to a server the
//! workspace no longer uses).

use anyhow::Result;

use crate::config::{self, RemoteConfig};
use crate::mutagen;
use crate::remote;
use crate::workspace;

pub fn set(
    host: Option<&str>,
    path: Option<String>,
    domain: Option<String>,
    no_verify: bool,
) -> Result<()> {
    let ws = workspace::locate()?;
    let mut cfg = config::load(&ws.root)?;

    let connection = host
        .map(str::to_string)
        .or_else(super::default_remote_host)
        .ok_or_else(|| {
            super::preflight(
                "`ws remote set` needs a host: pass one, set $WS_REMOTE_HOST, \
                 or `ws config set remote.host <host>`",
            )
        })?;
    // Fresh user input headed for ssh argv — refuse an option-shaped host now,
    // not when the stored config is next loaded.
    remote::ensure_connection_safe(&connection).map_err(|e| super::preflight(e.to_string()))?;

    crate::ui::header("ws-remote-set", &connection);

    // Keep the configured server path when re-pointing an already-remote
    // workspace and no --path was given; otherwise default to the workspace's
    // own dir name, written explicitly (like `ws init --remote`) so it's
    // discoverable in `.ws.json`. An empty --path (a script's unset variable)
    // is treated as absent — storing "" would anchor the server layout at the
    // filesystem root.
    let workspace_path = path
        .filter(|p| !p.is_empty())
        .or_else(|| cfg.remote.as_ref().and_then(|r| r.workspace_path.clone()))
        .unwrap_or_else(|| config::default_worktree_base(&ws.root));
    let new_remote = RemoteConfig::new(connection.clone(), Some(workspace_path));
    let base = new_remote.worktree_base(&ws.root);

    super::verify_remote_client(&new_remote, no_verify)?;

    cfg.remote = Some(new_remote);
    if domain.is_some() {
        cfg.domain = domain;
    }

    super::write_ws_json(&ws.root, &cfg)?;

    // Backfill sync for the worktrees that predate the switch. Config is
    // already written, so a partial failure here is recoverable by rerunning
    // `ws remote set` (the loop is idempotent) — report and continue, like a
    // hooksweep. Creates first, one flush pass after: `create` returns once the
    // daemon accepts the session, so the initial syncs run concurrently instead
    // of serializing N pushes.
    let mut created = Vec::new();
    let mut failed = false;
    // Enumerated from **git**, not from `.worktrees/` on disk — the shared
    // `workspace::worktrees` every op uses. A directory that git doesn't know as
    // a worktree (a failed `ws rm`'s residue, a hand-made copy) is not something
    // to push to a server; `ws clean` reports those instead. Unmanaged worktrees
    // (outside `.worktrees/`) are skipped too: they have no task dir, so there's
    // no server path to sync them to.
    for wt in workspace::worktrees(&ws.root, &cfg) {
        super::bail_if_interrupted()?;
        let Some(task) = wt.task else { continue };
        let repo = wt.repo;
        let session = mutagen::session_name(&task, &repo);

        // Drop the port forwards before rebuilding the sync — `set` also
        // re-points an *already-remote* workspace, and a forward outlives that
        // flip: its endpoint (`<connection>:tcp:localhost:<port>`) is baked in at
        // create time while its name (`ws-fwd-<branch>-<repo>-<port>`) carries no
        // host. So a forward a previous `ws up` made against the old server stays
        // live, and the next `ws up` re-creates it under the same name — which
        // `forward_create` reports as idempotent success — printing
        // `svc → localhost:<port>` while the socket still tunnels to the old host.
        // Terminating here makes the next `up` build them against the new
        // connection. Keyed off the still-present port record, mirroring `unset`;
        // a no-op when the workspace had no forwards.
        let sanitized = workspace::sanitize_branch(&task);
        for (_, port) in crate::ports::load(&ws.root, &sanitized, &repo) {
            let _ = mutagen::forward_terminate(&mutagen::forward_session_name(&task, &repo, port));
        }

        // Recreate every session from a clean slate. An existing one may point
        // anywhere — an older endpoint (and the rewritten config can no longer
        // tell), a survivor of a failed unset — and `create` masks a name
        // collision as success, so a create after a failed terminate would
        // report ok while the files keep syncing to the wrong server.
        // Terminating a session that doesn't exist is a no-op.
        if let Err(e) = mutagen::terminate(&session) {
            crate::ui::step(&format!("{task}/{repo}"), "Sync created")
                .fail(&format!("stale session not terminated: {e}"));
            failed = true;
            continue;
        }

        // `worktrees` only walks configured repos, so the entry is always found;
        // it's needed here for the repo's declared sync excludes.
        let Some(entry) = cfg.repos.iter().find(|r| r.name() == repo) else {
            continue;
        };
        let remote_path = remote::worktree_path(&base, &task, &repo);
        let ignores = mutagen::sync_ignores(entry.worktree_excludes(), &wt.path);
        let step = crate::ui::step(&format!("{task}/{repo}"), "Sync created");
        match mutagen::create(&session, &wt.path, &connection, &remote_path, &ignores) {
            Ok(()) => {
                created.push(session);
                step.ok();
            }
            Err(e) => {
                step.fail(&format!("{e}"));
                failed = true;
            }
        }
    }
    // Push the initial state so the server has the files before the first
    // `ws up`/`exec` — best-effort, like `ws new`.
    for session in &created {
        let _ = mutagen::flush(session);
    }

    // Show the server base too — on a re-point it may be a *retained* value
    // from the old config, and where the sync lands should never be implicit.
    let mut summary = vec![format!("remote  → {connection}:{base}")];
    if !created.is_empty() {
        summary.push(format!("sync backfilled for {} worktree(s)", created.len()));
    }
    crate::ui::summary_ok(&summary);
    // A stack that was running locally is now beyond `ws down` — every op
    // dispatches to the server from here on. The advice must be the sequence
    // that actually reaches it.
    crate::ui::note(
        "stacks that were running locally keep running and `ws down` now targets the server — \
         `ws remote unset`, `ws down`, then `ws remote set` again to stop them",
    );
    crate::ui::nudge("run `ws remote check` once the server's set up for full validation");

    if failed {
        return Err(super::op_failure("one or more sync sessions failed — fix and rerun `ws remote set`"));
    }
    Ok(())
}

pub fn unset() -> Result<()> {
    let ws = workspace::locate()?;
    let mut cfg = config::load(&ws.root)?;

    let Some(old) = cfg.remote.take() else {
        crate::ui::note("workspace is already local — nothing to unset");
        return Ok(());
    };

    crate::ui::header("ws-remote-unset", &old.connection);

    // Tear the file syncs down before the config flips, mirroring teardown's
    // "stop watching before you change the ground" order. Every worktree,
    // unfiltered — terminating a session that doesn't exist is a no-op, so the
    // wide net can only clean up more. Best-effort: a failed terminate is
    // reported but never blocks going local (`mutagen sync terminate ws-…` is
    // the documented escape hatch).
    //
    // Deliberately the **filesystem** walk, not `workspace::worktrees`: this is
    // the asymmetry between creating and destroying. `set` must be strict (only
    // sync what git calls a worktree), but `unset` must be broad — a directory
    // git doesn't know may still have a live session pointing at the server,
    // including one an older, laxer `set` created. Narrowing this to git would
    // strand exactly those.
    for (task, repo) in all_worktrees(&ws.root) {
        super::bail_if_interrupted()?;
        let session = mutagen::session_name(&task, &repo);
        let step = crate::ui::step(&format!("{task}/{repo}"), "Sync terminated");
        match mutagen::terminate(&session) {
            Ok(()) => step.ok(),
            Err(e) => step.fail(&format!("{e}")),
        }
        // Drop the port forwards too, before the config flip makes every future
        // path skip them (they're all guarded on `cfg.remote.is_some()`). Keyed
        // off the still-present port record. (`set` mirrors this for the same
        // reason — see the teardown in its backfill loop.)
        let sanitized = workspace::sanitize_branch(&task);
        for (_, port) in crate::ports::load(&ws.root, &sanitized, &repo) {
            let _ = mutagen::forward_terminate(&mutagen::forward_session_name(&task, &repo, port));
        }
    }

    super::write_ws_json(&ws.root, &cfg)?;

    crate::ui::summary_ok(&["workspace is local again".to_string()]);
    // Server-side state (stacks, Caddy routes, the generated override) is out
    // of reach once the block is gone — teardown would run locally. Same
    // actionable sequence as `set`'s note, in the other direction.
    crate::ui::note(&format!(
        "stacks and routes still on {} are left as-is — `ws remote set {}`, `ws down`, \
         then `ws remote unset` again to stop them",
        old.connection, old.connection
    ));
    Ok(())
}

/// Every existing worktree as `(task, repo)`, composed from the two canonical
/// `workspace` walkers so the ".worktrees layout" rules (symlink and non-UTF-8
/// handling) stay in one place. Task-dir names are already-sanitized branches;
/// `all_worktrees_for` re-sanitizes idempotently.
fn all_worktrees(root: &std::path::Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for task_dir in workspace::all_task_dirs(root) {
        let Some(task) = task_dir.file_name().and_then(|n| n.to_str()) else { continue };
        for (repo, _wt) in workspace::all_worktrees_for(root, task) {
            out.push((task.to_string(), repo));
        }
    }
    out
}
