//! Flipping a workspace between local and remote is more than a config edit:
//! worktrees that predate the switch need their file sync backfilled (going
//! remote) or torn down (going local), or `ws up`/`exec` targets a server with no
//! files — or mutagen keeps syncing to a server the workspace no longer uses.

use anyhow::Result;
use std::path::Path;
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
    // Fresh user input headed for ssh argv — refuse an option-shaped host now,
    // not when the stored config is next loaded.
    remote::ensure_connection_safe(&connection).map_err(|e| super::preflight(e.to_string()))?;

    crate::ui::header("ws-remote-set", &connection);

    let stranded = refuse_if_running(&cfg, &ws.root, force)?;

    // Read before the config changes: the flip is what invalidates this answer.
    let old_answer = expected_wildcard(cfg.remote.as_ref());

    // A re-point with no `--path` keeps the configured server path. An empty
    // `--path` (a script's unset variable) counts as absent — storing "" would
    // anchor the server layout at the filesystem root.
    let workspace_path = path
        .filter(|p| !p.is_empty())
        .or_else(|| cfg.remote.as_ref().and_then(|r| r.workspace_path.clone()))
        .unwrap_or_else(|| config::default_worktree_base(&ws.root));
    let new_remote = RemoteConfig::new(connection.clone(), Some(workspace_path));
    let base = new_remote.worktree_base(&ws.root);
    let new_answer = expected_wildcard(Some(&new_remote));

    super::verify_remote_client(&new_remote, no_verify)?;

    cfg.remote = Some(new_remote);
    if domain.is_some() {
        cfg.domain = domain;
    }

    super::write_ws_json(&ws.root, &cfg)?;

    // Backfill sync for the worktrees that predate the switch. The config is
    // already written, so a partial failure is recoverable by rerunning the whole
    // command — report and continue rather than abort.
    let mut created = Vec::new();
    let mut failed = false;
    // Enumerated from **git**, not from `.worktrees/` on disk: a directory git
    // doesn't know as a worktree (a failed `ws rm`'s residue, a hand-made copy) is
    // not something to push to a server. Unmanaged worktrees have no task dir and
    // so no server path to sync to.
    for wt in workspace::worktrees(&ws.root, &cfg) {
        super::bail_if_interrupted()?;
        let Some(task) = wt.task else { continue };
        let repo = wt.repo;
        let session = mutagen::session_name(&task, &repo);

        // A forward outlives a re-point: its endpoint is baked in at create time
        // but its name carries no host, so the next `ws up` "succeeds" at
        // re-creating it and prints `svc → localhost:<port>` while the socket
        // still tunnels to the old server. Terminate so `up` rebuilds them.
        let sanitized = workspace::sanitize_branch(&task);
        for (_, port) in crate::ports::load(&ws.root, &sanitized, &repo) {
            let _ = mutagen::forward_terminate(&mutagen::forward_session_name(&task, &repo, port));
        }

        // `create` masks a name collision as success, so a surviving session
        // pointing at an older endpoint would report ok while the files kept
        // syncing to the wrong server. A failed terminate has to be fatal for this
        // worktree; terminating a session that doesn't exist is a no-op.
        if let Err(e) = mutagen::terminate(&session) {
            crate::ui::step(&format!("{task}/{repo}"), "Sync created")
                .fail(&format!("stale session not terminated: {e}"));
            failed = true;
            continue;
        }

        // Always found — `worktrees` only walks configured repos.
        let Some(entry) = cfg.repos.iter().find(|r| r.name == repo) else {
            continue;
        };
        let remote_path = remote::worktree_path(&base, &task, &repo);
        let ignores = mutagen::sync_ignores(&entry.worktree_excludes, &wt.path);
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
    // Flushed in a second pass, not inside the loop: `create` returns as soon as
    // the daemon accepts the session, so the initial pushes overlap.
    for session in &created {
        let _ = mutagen::flush(session);
    }

    // The base is shown because on a re-point it may be retained from the old
    // config, and where the sync lands should never be implicit.
    let mut summary = vec![format!("remote  → {connection}:{base}")];
    if !created.is_empty() {
        summary.push(format!("sync backfilled for {} worktree(s)", created.len()));
    }
    crate::ui::summary_ok(&summary);
    // Only reachable via `--force`. A stack that was running is beyond `ws down`
    // from here on, so the advice has to be the sequence that reaches it again.
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

    // Before the `take` below: the stacks are on the server right now, and dropping
    // the remote block first would point the probe at the wrong machine.
    let stranded = refuse_if_running(&cfg, &ws.root, force)?;
    let old_answer = expected_wildcard(cfg.remote.as_ref());
    let old = cfg.remote.take().expect("checked above");

    // Torn down before the config flips, mirroring teardown's "stop watching
    // before you change the ground" order. Best-effort: a failed terminate is
    // reported but never blocks going local.
    //
    // Deliberately the **filesystem** walk, not the git-sourced one `set` uses: a
    // directory git doesn't know may still have a live session pointing at the
    // server, including one an older, laxer `set` created. Narrowing this to git
    // would strand exactly those.
    for (task, repo) in workspace::task_repo_pairs(&ws.root) {
        super::bail_if_interrupted()?;
        let session = mutagen::session_name(&task, &repo);
        let step = crate::ui::step(&format!("{task}/{repo}"), "Sync terminated");
        match mutagen::terminate(&session) {
            Ok(()) => step.ok(),
            Err(e) => step.fail(&format!("{e}")),
        }
        // Also before the flip, which makes every future path skip them (they're
        // guarded on `cfg.remote.is_some()`), and while the port record still exists.
        let sanitized = workspace::sanitize_branch(&task);
        for (_, port) in crate::ports::load(&ws.root, &sanitized, &repo) {
            let _ = mutagen::forward_terminate(&mutagen::forward_session_name(&task, &repo, port));
        }
    }

    super::write_ws_json(&ws.root, &cfg)?;

    crate::ui::summary_ok(&["workspace is local again".to_string()]);
    // Only reachable via `--force`. Server-side state is out of reach once the
    // block is gone — teardown would run locally.
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

/// The address `*.<domain>` should answer with, following the same rule as
/// `ws setup`. Derivation goes through `ssh -G`, which only parses config and
/// never connects, so this stays cheap and safe against an unreachable server;
/// when it fails the fallback *names* the server, so the note can still say which
/// machine the answer belongs to.
fn expected_wildcard(remote: Option<&RemoteConfig>) -> String {
    let Some(r) = remote else { return "127.0.0.1".to_string() };
    r.publish_address
        .as_deref()
        .map(str::trim)
        .filter(|a| {
            // A bind-any wildcard is the default bind, not somewhere a resolver
            // can point — same rule `ops::setup::routing_address` applies.
            a.parse::<std::net::IpAddr>().map(|ip| !ip.is_unspecified()).unwrap_or(false)
        })
        .map(str::to_string)
        .or_else(|| remote::routing_address(&r.connection))
        .unwrap_or_else(|| format!("the server `{}`", r.connection))
}

/// The wildcard's right answer is *derived* from where the stacks live, so a flip
/// invalidates it while leaving the resolver untouched — that is how a workspace
/// flipped back to local went on sending `*.<domain>` traffic to the offload
/// server. The DNS step is offered rather than run: `ws` never edits a resolver on
/// its own account, and when the offer can't be made or is declined the stale-DNS
/// note is the whole behavior, so a scripted flip is unchanged.
///
/// `offloading` is where the workspace points *after* the flip, which is what
/// decides the platform branch inside the offer.
fn settle_dns_flip(domain: Option<&str>, old: &str, new: &str, offloading: bool) {
    let Some(domain) = domain else { return };
    if old == new {
        return;
    }
    // The config is written and the syncs are backfilled by now, so a failure here
    // is the DNS commands failing and not the flip — fall through to the note.
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
    /// The probe couldn't reach a verdict — never collapsed into "nothing is
    /// running", which is precisely how things get stranded.
    Unknown,
}

/// Refuse the flip while anything is still running, returning whether `--force`
/// carried it past a positive or inconclusive finding.
///
/// Flipping a live workspace strands it: the stack stays where `ws` will no longer
/// look for it, so this enforces stop, flip, bring back up. Liveness is probed
/// **where the workspace points right now** — the side that would be stranded, so
/// the *old* server when re-pointing an already-remote workspace.
fn refuse_if_running(cfg: &config::WsConfig, root: &Path, force: bool) -> Result<bool> {
    // Only worktrees `ws` has a stack model for: a repo that is neither `kind:
    // host` nor a compose project has nothing to strand, and probing it would just
    // manufacture "unknown" verdicts on workspaces that can't be affected.
    // Unmanaged worktrees have no task dir, so no `ws down` reaches them either.
    let targets: Vec<workspace::Worktree> = workspace::worktrees(root, cfg)
        .into_iter()
        .filter(|wt| wt.task.is_some())
        .filter(|wt| cfg.is_host_repo(&wt.repo) || cfg.is_container_repo(&wt.repo, &wt.path))
        .collect();
    if targets.is_empty() {
        return Ok(false);
    }

    // Resolving a remote site fetches the server's `$HOME` over SSH, so this
    // doubles as the reachability verdict: unreachable means we cannot tell what
    // is running there, which is the `unset` case the refusal exists for.
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
        // A detached worktree has no branch, but its task dir name is the sanitized
        // branch, which is what every per-worktree name is keyed off anyway.
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
        // Grouped per branch because `ws down` takes a branch, and a workspace can
        // have several live at once.
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

/// Preserves first-seen branch order, so the message reads in the order the
/// worktrees were walked.
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

/// Refuse because liveness couldn't be determined. Deliberately *not* the same as
/// "nothing is running": guessing that is how a stack ends up on a machine `ws`
/// no longer talks to.
fn refuse_unknown(force: bool, reason: String, remedy: String) -> Result<bool> {
    refuse(
        force,
        format!("can't tell whether anything is still running: {reason}"),
        remedy,
    )
}

/// `remedy` is the ordinary way out and appears only in the refusal — repeating
/// "stop it first" at someone who just passed `--force` is noise. The `Ok(true)`
/// that `--force` yields is what tells the caller to print the recovery sequence.
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
        // `Crashed` counts as `Down`: a process that already died strands nothing,
        // and blocking on it would make the refusal say "still running", which is
        // simply false. All it leaves is a stale pid record — cleanup, not
        // stranding. `None` is the probe failing to run, which is genuinely unknown.
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

/// Every verdict rides one round trip, encoded in the exit status, because a
/// remote workspace pays SSH for each: `3` = no container runtime at the site,
/// `4` = the stack's directory isn't there. Both are sound "nothing is running"
/// inferences — no runtime can hold no containers, and a directory that was never
/// synced was never brought up — where a `compose ps` that runs and *fails* is
/// genuinely inconclusive and stays that way.
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
    // `COMPOSE_PROJECT_NAME` alone identifies this worktree's containers — the
    // generated override only carries ports/env, so `ps` needs no `COMPOSE_FILE`.
    // Every interpolated value is shell-quoted: `runtime` and the paths come from
    // a possibly cloned `.ws.json`.
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


