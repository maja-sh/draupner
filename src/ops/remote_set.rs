//! Flip a workspace between local and remote after `ws init`; worktree syncs
//! must be backfilled or torn down to match, and running stacks and stale DNS
//! are guarded because both are derived rather than declared.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::config::{self, RemoteConfig};
use crate::mutagen;
use crate::remote::{self, Site};
use crate::workspace;

pub fn set(
    host: Option<&str>,
    path: Option<String>,
    domain: Option<String>,
    no_verify: bool,
    force: bool,
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
    // Fresh user input headed for ssh argv — refuse an option-shaped host before storing it.
    remote::ensure_connection_safe(&connection).map_err(|e| super::preflight(e.to_string()))?;

    crate::ui::header("ws-remote-set", &connection);

    let stranded = refuse_if_running(&cfg, &ws.root, force)?;

    // Wildcard's correct answer is derived from where the stacks live; capture before flipping.
    let old_answer = expected_wildcard(cfg.remote.as_ref());

    // Empty --path is treated as absent — storing "" would anchor the server layout at /.
    let workspace_path = path
        .filter(|p| !p.is_empty())
        .or_else(|| cfg.remote.as_ref().and_then(|r| r.workspace_path.clone()))
        .unwrap_or_else(|| config::default_worktree_base(&ws.root));
    // Carry `publish_address` across the re-point: dropping it silently widens Caddy
    // back to every interface on the next `up`, the one direction it must never move alone.
    let new_remote = RemoteConfig {
        publish_address: cfg.remote.as_ref().and_then(|r| r.publish_address.clone()),
        ..RemoteConfig::new(connection.clone(), Some(workspace_path))
    };
    let base = new_remote.worktree_base(&ws.root);
    let new_answer = expected_wildcard(Some(&new_remote));

    super::verify_remote_client(&new_remote, no_verify)?;

    // Sync backfill runs *before* the cfg write: an interrupt mid-loop then
    // leaves `.ws.json` still pointing at the old host, so the user can re-run
    // from a consistent state instead of a workspace whose config points at a
    // new host that has none of its files. Every helper below reads the flip
    // target from local args (`connection`, `base`, `new_remote`) rather than
    // `cfg.remote`, so the in-memory cfg staying stale here is deliberate.
    let mut created = Vec::new();
    let mut failed = false;
    // git-sourced enumeration: unrecognised directories aren't ours to push to a server.
    for wt in workspace::worktrees(&ws.root, &cfg) {
        super::bail_if_interrupted()?;
        let Some(task) = wt.task else { continue };
        let repo = wt.repo;
        let session = mutagen::session_name(&task, &repo);

        // Forwards outlive the sync flip: their endpoint bakes in the old host but their name doesn't, so `forward_create` next `up` succeeds idempotently while the socket still tunnels to the old machine. Drop them keyed off the still-present port record.
        let sanitized = workspace::sanitize_branch(&task);
        for (_, port) in crate::ports::load(&ws.root, &sanitized, &repo) {
            let _ = mutagen::forward_terminate(&mutagen::forward_session_name(&task, &repo, port));
        }

        // Terminate first: `create` masks a name collision as success, so a survivor of an older endpoint would keep syncing to the wrong server. Terminating an absent one is a no-op.
        if let Err(e) = mutagen::terminate(&session) {
            crate::ui::step(&format!("{task}/{repo}"), "Sync created")
                .fail(&format!("stale session not terminated: {e}"));
            failed = true;
            continue;
        }

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
    // Best-effort initial push so files exist before the first `ws up`/`exec`.
    for session in &created {
        let _ = mutagen::flush(session);
    }

    // Cfg commits only after the syncs settle so a mid-flip crash leaves the
    // workspace pointing at a host that still has its files.
    cfg.remote = Some(new_remote);
    if domain.is_some() {
        cfg.domain = domain;
    }
    let step = crate::ui::step("cfg", ".ws.json");
    config::write_json_pretty(&ws.root.join(".ws.json"), &cfg)?;
    step.ok();

    let mut summary = vec![format!("remote  → {connection}:{base}")];
    if !created.is_empty() {
        summary.push(format!("sync backfilled for {} worktree(s)", created.len()));
    }
    crate::ui::summary_ok(&summary);
    if stranded {
        crate::ui::note(
            "stacks that were running keep running and `ws down` now targets the server — \
             `ws remote unset`, `ws down`, then `ws remote set` again to stop them",
        );
    }
    settle_dns_flip(cfg.domain.as_deref(), &old_answer, &new_answer, true);
    crate::ui::nudge("run `ws remote check` once the server's set up for full validation");

    if failed {
        return Err(super::op_failure("one or more sync sessions failed — fix and rerun `ws remote set`"));
    }
    Ok(())
}

pub fn unset(force: bool) -> Result<()> {
    let ws = workspace::locate()?;
    let mut cfg = config::load(&ws.root)?;

    if cfg.remote.is_none() {
        crate::ui::note("workspace is already local — nothing to unset");
        return Ok(());
    }

    crate::ui::header(
        "ws-remote-unset",
        cfg.remote.as_ref().map(|r| r.connection.as_str()).unwrap_or_default(),
    );

    // Must run before the `take` below: without the remote block the probe targets the wrong machine.
    let stranded = refuse_if_running(&cfg, &ws.root, force)?;
    let old_answer = expected_wildcard(cfg.remote.as_ref());
    let old = cfg.remote.take().expect("checked above");

    // Filesystem walk, not `workspace::worktrees`: teardown must be broad — a directory
    // git doesn't know may still have a live session, and narrowing to git would strand it.
    for (task, repo, _wt) in all_worktrees(&ws.root) {
        super::bail_if_interrupted()?;
        let session = mutagen::session_name(&task, &repo);
        let step = crate::ui::step(&format!("{task}/{repo}"), "Sync terminated");
        match mutagen::terminate(&session) {
            Ok(()) => step.ok(),
            Err(e) => step.fail(&format!("{e}")),
        }
        // Drop forwards before the config flip makes every future path skip them (guarded on `cfg.remote.is_some()`).
        let sanitized = workspace::sanitize_branch(&task);
        for (_, port) in crate::ports::load(&ws.root, &sanitized, &repo) {
            let _ = mutagen::forward_terminate(&mutagen::forward_session_name(&task, &repo, port));
        }
    }

    let step = crate::ui::step("cfg", ".ws.json");
    config::write_json_pretty(&ws.root.join(".ws.json"), &cfg)?;
    step.ok();

    crate::ui::summary_ok(&["workspace is local again".to_string()]);
    if stranded {
        crate::ui::note(&format!(
            "stacks and routes still on {} are left as-is — `ws remote set {}`, `ws down`, \
             then `ws remote unset` again to stop them",
            old.connection, old.connection
        ));
    }
    settle_dns_flip(cfg.domain.as_deref(), &old_answer, "127.0.0.1", false);
    Ok(())
}

/// Falls back to naming the server when derivation fails so the note can still identify the machine.
fn expected_wildcard(remote: Option<&RemoteConfig>) -> String {
    let Some(r) = remote else { return "127.0.0.1".to_string() };
    r.publish_address
        .as_deref()
        .map(str::trim)
        .filter(|a| {
            // Bind-any wildcards are a default bind, not somewhere a resolver can point.
            a.parse::<std::net::IpAddr>().map(|ip| !ip.is_unspecified()).unwrap_or(false)
        })
        .map(str::to_string)
        .or_else(|| remote::routing_address(&r.connection))
        .unwrap_or_else(|| format!("the server `{}`", r.connection))
}

/// `offloading` is where the workspace points after the flip, deciding the platform branch.
fn settle_dns_flip(domain: Option<&str>, old: &str, new: &str, offloading: bool) {
    let Some(domain) = domain else { return };
    if old == new {
        return;
    }
    // Failure here is the DNS commands, not the flip; fall through to the note.
    match crate::setup::offer_dns_step(domain, new, offloading) {
        Ok(true) => return,
        Ok(false) => {}
        Err(e) => crate::ui::note(&format!("re-pointing DNS didn't complete ({e:#})")),
    }
    crate::ui::note(&format!(
        "DNS is now stale: `*.{domain}` should resolve to {new}, not {old} — \
         re-run `ws setup` to re-point it (`ws` never edits your resolver itself)"
    ));
}

#[derive(PartialEq)]
enum Live {
    Up,
    Down,
    /// Never collapsed into Down — that is precisely how things get stranded.
    Unknown,
}

/// Probes where the workspace points now — the side that would be stranded. Returns whether
/// --force overrode a positive or inconclusive finding.
fn refuse_if_running(cfg: &config::WsConfig, root: &Path, force: bool) -> Result<bool> {
    // Filter to repos ws has a stack model for: anything else would only manufacture "unknown".
    let targets: Vec<workspace::Worktree> = workspace::worktrees(root, cfg)
        .into_iter()
        .filter(|wt| wt.task.is_some())
        .filter(|wt| cfg.is_host_repo(&wt.repo) || cfg.is_container_repo(&wt.repo, &wt.path))
        .collect();
    if targets.is_empty() {
        return Ok(false);
    }

    // Resolving a remote site fetches `$HOME` over SSH, so this doubles as the reachability verdict.
    let site = match Site::resolve(cfg) {
        Ok(s) => s,
        Err(_) => {
            let conn = cfg.remote.as_ref().map(|r| r.connection.as_str()).unwrap_or("the server");
            return refuse_unknown(
                force,
                format!("`{conn}` isn't reachable over SSH"),
                "Fix the connection and re-run.".to_string(),
            );
        }
    };

    let mut up: Vec<(String, String)> = Vec::new();
    let mut unknown: Vec<(String, String)> = Vec::new();
    for wt in &targets {
        super::bail_if_interrupted()?;
        // Detached worktrees have no branch; task dir name is the sanitized branch.
        let branch = wt
            .branch
            .clone()
            .or_else(|| wt.task.clone())
            .unwrap_or_default();
        match liveness(cfg, &site, root, wt, &branch) {
            Live::Up => up.push((branch, wt.repo.clone())),
            Live::Unknown => unknown.push((branch, wt.repo.clone())),
            Live::Down => {}
        }
    }

    if !up.is_empty() {
        let branches = by_branch(&up);
        let what = branches
            .iter()
            .map(|(b, repos)| format!("{b} ({})", repos.join(", ")))
            .collect::<Vec<_>>()
            .join(", ");
        let cmds = branches
            .iter()
            .map(|(b, _)| format!("`ws down {b}`"))
            .collect::<Vec<_>>()
            .join(", ");
        return refuse(
            force,
            format!("still running: {what}"),
            format!("Stop it first ({cmds}), then re-run this flip."),
        );
    }
    if !unknown.is_empty() {
        let what = by_branch(&unknown)
            .iter()
            .map(|(b, repos)| format!("{b} ({})", repos.join(", ")))
            .collect::<Vec<_>>()
            .join(", ");
        return refuse_unknown(
            force,
            format!("no answer from {what}"),
            "Re-run once it can be probed.".to_string(),
        );
    }
    Ok(false)
}

/// Preserves first-seen branch order.
fn by_branch(pairs: &[(String, String)]) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for (branch, repo) in pairs {
        match out.iter_mut().find(|(b, _)| b == branch) {
            Some((_, repos)) => repos.push(repo.clone()),
            None => out.push((branch.clone(), vec![repo.clone()])),
        }
    }
    out
}

/// Deliberately not the same as "nothing is running": that guess strands stacks.
fn refuse_unknown(force: bool, reason: String, remedy: String) -> Result<bool> {
    refuse(
        force,
        format!("can't tell whether anything is still running: {reason}"),
        remedy,
    )
}

fn refuse(force: bool, finding: String, remedy: String) -> Result<bool> {
    if force {
        crate::ui::note(&format!("--force: {finding} — flipping anyway"));
        return Ok(true);
    }
    Err(super::preflight(format!(
        "refusing to flip — {finding}. {remedy} Flipping now leaves the stack where `ws` \
         will no longer look for it (`ws down` follows the config, not the containers); \
         pass --force to flip anyway."
    )))
}

fn liveness(
    cfg: &config::WsConfig,
    site: &Site,
    root: &Path,
    wt: &workspace::Worktree,
    branch: &str,
) -> Live {
    let repo = &wt.repo;
    let sanitized = workspace::sanitize_branch(branch);
    if cfg.is_host_repo(repo) {
        // Crashed maps to Down: a dead process strands nothing, and reporting it as
        // "still running" would be the exact miscommunication this refusal fixes.
        let ports: Vec<u16> = crate::ports::load(root, &sanitized, repo).into_values().collect();
        let name = crate::supervise::proc_name(branch, repo);
        return match crate::supervise::health(site, &name, &ports) {
            Some(crate::supervise::Health::NotStarted) | Some(crate::supervise::Health::Crashed) => {
                Live::Down
            }
            Some(_) => Live::Up,
            None => Live::Unknown,
        };
    }
    compose_liveness(cfg, site, root, wt, &sanitized)
}

/// One round trip carries every verdict via exit status (3 = no runtime, 4 = no dir)
/// because remote pays SSH per probe; both are sound Down inferences, not guesses.
fn compose_liveness(
    cfg: &config::WsConfig,
    site: &Site,
    root: &Path,
    wt: &workspace::Worktree,
    sanitized: &str,
) -> Live {
    let repo = &wt.repo;
    let dir = match (site, cfg.remote.as_ref()) {
        (Site::Remote { .. }, Some(r)) => remote::worktree_path(
            &site.abs_base(&r.worktree_base(root)),
            sanitized,
            repo,
        ),
        _ => wt.path.to_string_lossy().into_owned(),
    };
    // `ps` needs no COMPOSE_FILE since the generated override only carries ports/env.
    // Every interpolated value is shell-quoted: `runtime` comes from a possibly cloned `.ws.json`.
    let script = format!(
        "command -v {rt} >/dev/null 2>&1 || exit 3\n\
         cd {dir} 2>/dev/null || exit 4\n\
         COMPOSE_PROJECT_NAME={proj} {rt} compose ps -q",
        rt = remote::shell_quote(cfg.runtime()),
        dir = remote::shell_quote(&dir),
        proj = remote::shell_quote(&crate::compose::project_name(sanitized, repo)),
    );
    let Ok(out) = site.shell(&script).stdin(Stdio::null()).stderr(Stdio::null()).output() else {
        return Live::Unknown;
    };
    match out.status.code() {
        Some(0) => {
            if String::from_utf8_lossy(&out.stdout).split_whitespace().next().is_some() {
                Live::Up
            } else {
                Live::Down
            }
        }
        Some(3 | 4) => Live::Down,
        _ => Live::Unknown,
    }
}

fn all_worktrees(root: &std::path::Path) -> Vec<(String, String, PathBuf)> {
    let mut out = Vec::new();
    for task_dir in workspace::all_task_dirs(root) {
        let Some(task) = task_dir.file_name().and_then(|n| n.to_str()) else { continue };
        for (repo, wt) in workspace::all_worktree_dirs_for(root, task) {
            out.push((task.to_string(), repo, wt));
        }
    }
    out
}
