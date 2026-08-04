//! `ws remote set` / `ws remote unset` — flip a workspace between local and
//! remote **after** `ws init` (which refuses to re-run on an existing
//! `.ws.json`). The flip is workspace-level, and it's more than a config edit:
//! worktrees that predate the switch need their file sync backfilled (going
//! remote) or torn down (going local), else `ws up`/`exec` would target a
//! server that has no files (or mutagen would keep syncing to a server the
//! workspace no longer uses).
//!
//! Two things guard the flip, because both of the states it changes are derived
//! rather than declared. [`refuse_if_running`] refuses while a stack is up, since
//! `ws down` follows the config and not the containers. [`settle_dns_flip`]
//! handles the wildcard's correct answer having just changed — it offers `ws
//! setup`'s own DNS step and falls back to saying what's stale, so DNS is still
//! only ever rewritten with consent.

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
    // Fresh user input headed for ssh argv — refuse an option-shaped host now,
    // not when the stored config is next loaded.
    remote::ensure_connection_safe(&connection).map_err(|e| super::preflight(e.to_string()))?;

    crate::ui::header("ws-remote-set", &connection);

    // Pre-flight before anything is touched: a stack still running where the
    // workspace points *now* would be stranded by the flip.
    let stranded = refuse_if_running(&cfg, &ws.root, force)?;

    // The wildcard's correct answer is derived from where the stacks live, so the
    // flip about to happen invalidates it. Read it before the config changes.
    let old_answer = expected_wildcard(cfg.remote.as_ref());

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
    // Carry a pinned `publish_address` across the re-point for the same reason
    // `workspace_path` is carried: it is deliberate configuration the user wrote,
    // and `RemoteConfig::new` leaves it unset. Dropping it silently widens Caddy
    // back to every interface on the next `up` — the one direction this setting
    // must never move on its own.
    let new_remote = RemoteConfig {
        publish_address: cfg.remote.as_ref().and_then(|r| r.publish_address.clone()),
        ..RemoteConfig::new(connection.clone(), Some(workspace_path))
    };
    let base = new_remote.worktree_base(&ws.root);
    let new_answer = expected_wildcard(Some(&new_remote));

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
    // Only reachable via `--force` now: the pre-flight refuses an unforced flip
    // with anything running. A stack that was running is beyond `ws down` from
    // here on — every op dispatches to the server — so the advice must be the
    // sequence that actually reaches it.
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

    // Pre-flight against the *server*, which is where the stacks are right now.
    // Taking the remote block out of `cfg` first would point the probe at the
    // wrong machine, so it happens before the `take` below.
    let stranded = refuse_if_running(&cfg, &ws.root, force)?;
    let old_answer = expected_wildcard(cfg.remote.as_ref());
    let old = cfg.remote.take().expect("checked above");

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
    for (task, repo, _wt) in all_worktrees(&ws.root) {
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
    // Only reachable via `--force`. Server-side state (stacks, Caddy routes, the
    // generated override) is out of reach once the block is gone — teardown would
    // run locally. Same actionable sequence as `set`'s note, in the other direction.
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

/// The address `*.<domain>` should answer with for a given workspace state:
/// loopback when local, otherwise the server's — the pinned `publish_address` when
/// it names a concrete address, else derived from the SSH connection (`ssh -G` +
/// DNS, the same rule `ws setup` follows). Falls back to *naming* the server when
/// derivation fails, so the note can still say which machine the answer belongs to
/// even when it can't say which address. `ssh -G` only parses config — it never
/// connects — so this is cheap and safe against an unreachable server.
fn expected_wildcard(remote: Option<&RemoteConfig>) -> String {
    let Some(r) = remote else { return "127.0.0.1".to_string() };
    r.publish_address
        .as_deref()
        .map(str::trim)
        .filter(|a| {
            // A bind-any wildcard is the *default bind*, not somewhere a resolver
            // can point — same rule `ops::setup::routing_address` applies.
            a.parse::<std::net::IpAddr>().map(|ip| !ip.is_unspecified()).unwrap_or(false)
        })
        .map(str::to_string)
        .or_else(|| remote::routing_address(&r.connection))
        .unwrap_or_else(|| format!("the server `{}`", r.connection))
}

/// Settle the wildcard's correct answer after a flip changed it: offer the DNS
/// step, and say what's stale if it wasn't taken.
///
/// The right answer is *derived* from where the stacks live — loopback when local,
/// the server's address when remote — so a flip silently invalidates it while
/// leaving the resolver untouched. That is exactly how a workspace flipped back to
/// local kept resolving `*.<domain>` to the offload server: a local dev server
/// proxied `/api` to that hostname, the request crossed the network to a machine
/// the user believed she had stopped using, and its Caddy answered with the
/// "No stack is running here" page.
///
/// Never an action taken silently — but a consented one is fine, and this is a
/// place where a `[R]un / [S]kip` beats homework: re-pointing the wildcard is the
/// same step `ws setup` walks, and having to re-run the whole setup command every
/// time you switch sides is friction with no transparency dividend.
/// [`crate::setup::offer_dns_step`] runs *that* step, prompt and all; `ws` still
/// never edits a resolver on its own account. When it isn't offered (no tty, a
/// platform whose privileged bits live in `server-setup/`) or is declined, the
/// note is what it always was, so a scripted flip is unchanged.
///
/// `offloading` is where the workspace points *after* the flip — `set` true,
/// `unset` false — which is what decides the platform branch inside the offer.
fn settle_dns_flip(domain: Option<&str>, old: &str, new: &str, offloading: bool) {
    let Some(domain) = domain else { return };
    if old == new {
        return; // re-pointing at the same address changes nothing to re-point
    }
    // A failure here is the DNS commands failing, not the flip: the config is
    // already written and the syncs are already backfilled. Report it and fall
    // through to the note, which is now exactly the advice the user needs.
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

/// Whether a worktree's stack is up, as far as the pre-flight can tell.
#[derive(PartialEq)]
enum Live {
    Up,
    Down,
    /// The probe couldn't reach a verdict — never collapsed into "nothing is
    /// running", which is precisely how things get stranded.
    Unknown,
}

/// Refuse the flip while anything is still running, and report whether `--force`
/// carried it past a positive or inconclusive finding.
///
/// Flipping a live workspace strands it: after `set`, a stack running locally is
/// beyond `ws down` (every op dispatches to the server from then on); after
/// `unset`, the server's containers, routes and override are equally out of reach.
/// This used to be a documented footgun with a recovery note printed *after* the
/// damage. It's a pre-flight now, so the working order — stop the stack, flip,
/// bring it back up — is the one the tool enforces.
///
/// Liveness is probed **where the workspace points right now**, which is the side
/// that would be stranded: locally for a local workspace, and on the *old* server
/// when re-pointing an already-remote one.
fn refuse_if_running(cfg: &config::WsConfig, root: &Path, force: bool) -> Result<bool> {
    // The same git-sourced enumeration `set`'s sync backfill walks, filtered to
    // worktrees `ws` has a stack model for. A repo that is neither `kind: host`
    // nor a compose project has nothing `ws` starts or supervises, so there is
    // nothing for the flip to strand — and probing it would only manufacture
    // "unknown" verdicts on workspaces that can't be affected. Unmanaged
    // worktrees are skipped for the same reason `set` skips them: with no task
    // dir there's no `ws down <branch>` that would even reach them.
    let targets: Vec<workspace::Worktree> = workspace::worktrees(root, cfg)
        .into_iter()
        .filter(|wt| wt.task.is_some())
        .filter(|wt| cfg.is_host_repo(&wt.repo) || cfg.is_container_repo(&wt.repo, &wt.path))
        .collect();
    if targets.is_empty() {
        return Ok(false);
    }

    // Resolving a remote site fetches the server's `$HOME` over SSH, so this is
    // also the reachability verdict. An unreachable server means we cannot tell
    // what's running there — the `unset` case the refusal exists for.
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
        // A detached worktree has no branch; its task dir name is the sanitized
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
        // Name what's up and the exact command that clears it — per branch, since
        // `ws down` takes a branch and a workspace can have several live at once.
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

/// Group `(branch, repo)` pairs into `(branch, repos)`, preserving first-seen
/// branch order so the message reads in the order the worktrees were walked.
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

/// Turn a finding into the refusal, or into `Ok(true)` when `--force` overrides
/// (matching the `--no-verify` precedent on these ops: `ws` states the risk, the
/// user decides). `remedy` is the ordinary way out and belongs only in the
/// refusal — repeating "stop it first" at someone who just said `--force` is
/// noise. The caller uses `true` to print the recovery sequence.
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

/// Probe one worktree's stack at `site`.
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
        // A host repo's stack IS a ws-owned detached process, so ws can answer for
        // it directly.
        //
        // `Crashed` does NOT block. The refusal exists to stop a *running* stack
        // being stranded on a machine `ws` will no longer look at; a process that
        // already died strands nothing. Blocking on it would also make the refusal
        // say "still running", which for a crashed process is simply false — and a
        // message that misstates the situation is the exact failure mode this whole
        // branch is fixing. What it leaves behind is a stale pid record, which is
        // cleanup, not stranding.
        //
        // `None` is the probe failing to run, which is genuinely unknown.
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

/// Whether a container repo's compose project has any container at `site`, via
/// `compose ps -q`.
///
/// One round trip carries every verdict, encoded in the exit status, because a
/// remote workspace pays SSH for each: `3` = no container runtime at the site and
/// `4` = the stack's directory isn't there. Both are sound "nothing is running"
/// inferences rather than guesses — no runtime can hold no containers, and a
/// directory that was never synced was never brought up. A `compose ps` that runs
/// and *fails* is genuinely inconclusive, and stays that way.
fn compose_liveness(
    cfg: &config::WsConfig,
    site: &Site,
    root: &Path,
    wt: &workspace::Worktree,
    sanitized: &str,
) -> Live {
    let repo = &wt.repo;
    // The stack's directory at the site: the local worktree, or the synced server
    // dir for a remote workspace.
    let dir = match (site, cfg.remote.as_ref()) {
        (Site::Remote { .. }, Some(r)) => remote::worktree_path(
            &site.abs_base(&r.worktree_base(root)),
            sanitized,
            repo,
        ),
        _ => wt.path.to_string_lossy().into_owned(),
    };
    // `COMPOSE_PROJECT_NAME` is what identifies this worktree's containers; the
    // generated override only carries ports/env, so `ps` needs no `COMPOSE_FILE`.
    // Every interpolated value is shell-quoted — `runtime` comes from a possibly
    // cloned `.ws.json`, same posture as `compose::remote_compose_script`.
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

/// Every existing worktree as `(task, repo, path)`, composed from the two
/// canonical `workspace` walkers so the ".worktrees layout" rules (symlink and
/// non-UTF-8 handling) stay in one place. Task-dir names are already-sanitized
/// branches; `all_worktree_dirs_for` re-sanitizes idempotently.
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
