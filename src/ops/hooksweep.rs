//! Shared driver for `ws up` and `ws down` — the symmetric hook-runner pair.
//!
//! Both ops do the same thing: for every worktree of a branch, run one hook
//! event (`up` or `down`), isolating per-repo failures so one repo's broken
//! hook doesn't strand the others. They differ only in the event name, header,
//! and closing summary — those stay in the op modules.

use anyhow::Result;

use crate::config;
use crate::hooks;
use crate::inject;
use crate::remote::Site;
use crate::workspace;

/// Outcome of a sweep, for the caller to render its own summary from.
pub struct SweepResult {
    /// Repos whose hook ran and exited 0.
    pub ran: Vec<String>,
    /// Repos that had a worktree but no hook for this event (skipped, dim line).
    pub skipped: Vec<String>,
    /// Repos whose hook exited non-zero. Non-empty ⇒ the op should exit 1.
    pub failed: Vec<String>,
}

impl SweepResult {
    /// Finish a step and file the repo under `ran` or `failed`. The shared
    /// tail every per-repo action (container hook, host up/down) ends with.
    fn record(&mut self, repo: &str, step: crate::ui::Step, res: Result<()>) {
        match res {
            Ok(()) => {
                step.ok();
                self.ran.push(repo.to_string());
            }
            Err(e) => {
                step.fail(&format!("{e}"));
                self.failed.push(repo.to_string());
            }
        }
    }

    /// Move a repo already filed under `ran` into `failed`. Used when a repo's
    /// hook succeeded but installing its Caddy route failed — the containers are
    /// up, but with the host publish `!reset` away the HTTP service is
    /// unreachable, so the repo hasn't really come up.
    fn demote(&mut self, repo: &str) {
        self.ran.retain(|r| r != repo);
        if !self.failed.iter().any(|r| r == repo) {
            self.failed.push(repo.to_string());
        }
    }
}

/// Run `event`'s hook across every worktree of `branch`. Prints the op header
/// (`◐ ws-<op> <branch>`), a step line per repo, and streams hook output. A
/// missing hook is a dim skip; a failing hook is reported but does not stop the
/// remaining repos.
///
/// Hard-fails with a preflight error (exit 2) when the branch has no worktrees.
pub fn sweep(branch: &str, event: &str, op: &str) -> Result<SweepResult> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;
    let sanitized = workspace::sanitize_branch(branch);
    // Where the stack lives — locally, or the server for a remote workspace.
    // Resolving a remote site fetches the server `$HOME` up front, so an
    // unreachable server fails here with a clear message before any bring-up.
    let site = Site::resolve(&cfg)?;

    // Iterate repos in config order (deterministic output), keeping only those
    // that actually have a worktree for this branch. Repos not part of the task
    // are silently absent — they were never brought up.
    let targets: Vec<(String, std::path::PathBuf)> = cfg
        .repos
        .iter()
        .map(|r| r.name())
        .filter_map(|name| {
            workspace::worktree_for(&ws.root, branch, name).map(|wt| (name.to_string(), wt))
        })
        .collect();

    if targets.is_empty() {
        return Err(super::preflight(format!(
            "no worktrees for {branch}; run `ws new` first"
        )));
    }

    crate::ui::header(op, branch);

    let mut result = SweepResult { ran: Vec::new(), skipped: Vec::new(), failed: Vec::new() };

    // `up` activates the stack (allocate ports, render templates); `down` is a
    // teardown-side event that only reads what's already allocated.
    let phase = if event == "up" { inject::Phase::Activate } else { inject::Phase::Inspect };

    // Bring up the shared routing infra once before the loop, best-effort. It's
    // NOT fatal here: a sweep whose repos are all raw-TCP (no HTTP service) joins
    // no `ws-net` and drops no host port, so it needs neither Caddy nor the
    // network — a broken Caddy shouldn't block it. Fatality lands precisely where
    // routing is load-bearing instead: an HTTP container whose `ws-net` is missing
    // fails its own `compose up`, and one whose route won't install is failed
    // below (its host publish is `!reset` away, so an uninstalled route ⇒
    // unreachable). `ensure_routing` owns the Caddy-side sequencing + the
    // DNS-not-ready hint; we only surface what it returns.
    if event == "up" {
        if let Some(domain) = cfg.domain.as_deref().filter(|_| cfg.routing_active()) {
            // `ensure_routing` already probes DNS; its result *is* the answer
            // (`Some(hint)` ⇒ wildcard doesn't resolve). Reuse it so the auto-offer
            // below doesn't resolve DNS a second time.
            let configured = cfg.remote.as_ref().and_then(|r| r.publish_address.as_deref());
            let publish = crate::caddy::publish_address(&site, configured);
            // Say it out loud. The default is right (offload is pointless if the
            // client can't reach it), but on a box with a public interface it also
            // serves every worktree to the internet, and nothing else here would
            // tell you that.
            if site.is_remote() && configured.is_none() {
                crate::ui::note(
                    "publishing on all interfaces — set `remote.publish_address` in .ws.json \
                     to pin it to the server's tailnet/LAN address",
                );
            }
            let dns_ok = match crate::caddy::ensure_routing(&site, cfg.runtime(), domain, &publish) {
                Ok(hint) => {
                    if let Some(h) = &hint {
                        crate::ui::note(h);
                    }
                    hint.is_none()
                }
                Err(e) => {
                    crate::ui::note(&format!("routing infra not ready: {e:#}"));
                    false
                }
            };
            // Auto-offer setup when a privileged bit (DNS / CA trust / :443) is
            // missing — a non-mutating nudge to run `ws setup`. Skipped under an
            // externally-managed Caddy (BYO / the test seam).
            //
            // `ws setup` handles a remote workspace too (the client's DNS + the
            // server's CA), but the full probe isn't worth its cost on the remote
            // up path: it would need the server address resolved and its CA check
            // dials the server. `dns_ok` is already in hand for free and is the bit
            // that usually isn't wired, so remote nudges on that alone.
            if !crate::caddy::externally_managed() {
                let incomplete = if site.is_remote() {
                    !dns_ok
                } else {
                    crate::setup::probe(&site, cfg.runtime(), domain, "127.0.0.1", Some(dns_ok))
                        .privileged_gaps()
                };
                if incomplete {
                    crate::ui::nudge(
                        "`ws setup` — some routing setup (DNS / CA trust) is incomplete",
                    );
                }
            }
        }
    }

    for (repo, wt) in &targets {
        super::bail_if_interrupted()?;

        // Client-side prep before the stack comes up. `prepare` runs on THIS
        // machine even for a remote workspace — the one hook that doesn't follow
        // the stack to the server — so it can stage data from client-side
        // resources (e.g. dump the main checkout's db) into the local worktree.
        // The flush inside the up dispatch then carries that output to the server,
        // where `up` consumes it. A failed prepare fails the repo and skips its up:
        // no point bringing a stack up whose seed never staged.
        if event == "up" && !run_prepare(&cfg, &ws.root, wt, &sanitized, repo, &mut result) {
            continue;
        }

        // Host processes are supervised as a detached `ws`-owned process, not run as a
        // blocking hook — `up` launches it (detached, survives `ws` exiting), `down`
        // stops. Their `up` script binds the injected `WS_PORT_*`.
        if cfg.is_host_repo(repo) {
            sweep_host(&cfg, &site, &ws.root, wt, branch, &sanitized, repo, event, phase, &mut result);
            continue;
        }

        // A remote workspace runs the container lifecycle on the server over SSH
        // (in the synced worktree); the local path streams the hook/script here.
        if site.is_remote() {
            sweep_container_remote(
                &cfg, &site, &ws.root, wt, branch, &sanitized, repo, event, phase, &mut result,
            );
            continue;
        }

        let Some(action) = hooks::resolve(&cfg, &ws.root, repo, event, cfg.is_container_repo(repo, wt)) else {
            crate::ui::skip(repo, &format!("no {event} hook"));
            result.skipped.push(repo.clone());
            continue;
        };

        // Compute the injection context (ports/hosts/env) and render templates.
        // A failure here is isolated per-repo, like a failing hook — it doesn't
        // strand the other repos in the sweep.
        let extra_env = match inject::prepare(&cfg, &site, &ws.root, wt, &sanitized, repo, phase) {
            Ok(env) => env,
            Err(e) => {
                crate::ui::step(repo, "inject").fail(&format!("{e}"));
                result.failed.push(repo.clone());
                continue;
            }
        };

        let step = crate::ui::step(repo, &format!("{event} hook"));
        let res = hooks::run_action(&ws.root, wt, repo, &sanitized, event, &action, &extra_env);
        let up_ok = res.is_ok();
        result.record(repo, step, res);

        // Sync Caddy routes: add after a successful `up`, remove on `down`. A
        // container's HTTP services are reachable ONLY through Caddy now (the
        // override `!reset`s their host publish), so an `up` route that won't
        // install means the running container is unreachable — fail the repo.
        if cfg.routing_active() && (event == "down" || up_ok) {
            let routed_ok = sync_routes(&cfg, &site, &ws.root, &sanitized, repo, event);
            if event == "up" && !routed_ok {
                result.demote(repo);
            }
        }
    }

    Ok(result)
}

/// Run a repo's client-side `prepare` hook (opt-in) locally, ahead of `up`.
///
/// Unlike every other lifecycle hook, `prepare` runs on the **client** regardless
/// of where the stack lives — so a remote workspace can stage data that only
/// exists on the client (the main checkout's live db, local certs) into the
/// worktree, which the up dispatch's mutagen flush then ships to the server. The
/// site is therefore always `Local` and the worktree the local one; `WS_REMOTE`
/// still reports the workspace's remoteness (it's keyed off `cfg.remote`), so one
/// script can branch dump-for-sync vs a same-host copy. Phase is `Inspect` —
/// prepare stages data, it doesn't allocate ports or render templates.
///
/// Returns `false` (and files the repo under `failed`) when the hook fails, so the
/// caller skips the up; `true` on success or when there's no `prepare` hook (the
/// common case — it never prints a skip line).
fn run_prepare(
    cfg: &config::WsConfig,
    root: &std::path::Path,
    wt: &std::path::Path,
    sanitized: &str,
    repo: &str,
    result: &mut SweepResult,
) -> bool {
    let Some(action) = hooks::resolve(cfg, root, repo, "prepare", cfg.is_container_repo(repo, wt))
    else {
        return true;
    };
    let extra_env =
        inject::prepare(cfg, &Site::Local, root, wt, sanitized, repo, inject::Phase::Inspect)
            .unwrap_or_default();
    let step = crate::ui::step(repo, "prepare hook");
    match hooks::run_action(root, wt, repo, sanitized, "prepare", &action, &extra_env) {
        Ok(()) => {
            step.ok();
            true
        }
        Err(e) => {
            step.fail(&format!("{e}"));
            result.failed.push(repo.to_string());
            false
        }
    }
}

/// Handle one container repo on the server for a remote workspace: run its
/// declarative `up`/`down` script over SSH (in the synced worktree, with the
/// injected env), then sync Caddy routes on the server. Mirrors the local
/// container path but the script and routing run where the stack lives.
///
/// A legacy `.ws-hooks/<repo>/<event>` file lives on the *host* and can't run on
/// the server, so a repo with only that (no declared `scripts` entry) is a skip:
/// declarative `scripts` is the remote contract.
#[allow(clippy::too_many_arguments)]
fn sweep_container_remote(
    cfg: &config::WsConfig,
    site: &Site,
    root: &std::path::Path,
    wt: &std::path::Path,
    branch: &str,
    sanitized: &str,
    repo: &str,
    event: &str,
    phase: inject::Phase,
    result: &mut SweepResult,
) {
    let Some(cmd) = cfg.command_for(repo, event, cfg.is_container_repo(repo, wt)) else {
        crate::ui::skip(repo, &format!("no {event} script"));
        result.skipped.push(repo.to_string());
        return;
    };

    // On `up`, `prepare` derives services + writes the override by reading the
    // worktree *on the server* — so push our latest edits first, else a just-
    // edited compose file would derive stale. (`run_remote_event` flushes again
    // before the script, cheaply; `down`/Inspect doesn't derive but the flush is
    // harmless.) Best-effort: the session may not exist yet, or mutagen be absent.
    let _ = crate::mutagen::flush(&crate::mutagen::session_name(branch, repo));

    let extra_env = match inject::prepare(cfg, site, root, wt, sanitized, repo, phase) {
        Ok(env) => env,
        Err(e) => {
            crate::ui::step(repo, "inject").fail(&format!("{e}"));
            result.failed.push(repo.to_string());
            return;
        }
    };

    let r = cfg.remote.as_ref().expect("remote site ⇒ a remote config");
    let step = crate::ui::step(repo, &format!("{event} hook"));
    let res = super::run_remote_event(site, r, root, branch, sanitized, repo, event, &cmd, &extra_env);
    let up_ok = res.is_ok();
    result.record(repo, step, res);

    if cfg.routing_active() && (event == "down" || up_ok) {
        let routed_ok = sync_routes(cfg, site, root, sanitized, repo, event);
        if event == "up" && !routed_ok {
            result.demote(repo);
        }
    }
    // Forward this repo's raw-TCP ports (db, redis, …) to localhost, so a DB GUI
    // or CLI on this machine reaches the server's containers unchanged.
    if event == "down" || up_ok {
        sync_forwards(cfg, root, branch, sanitized, repo, event);
    }
}

/// Add (`up`) or remove (`down`) the Caddy routes for a repo's HTTP services.
/// Returns whether the sync succeeded: always `true` for `down` (removal is
/// best-effort — teardown is infallible by design) and for a repo with no HTTP
/// routes; on `up`, `false` if any route failed to install (the container caller
/// fails the repo, since an HTTP container has no host-port fallback).
fn sync_routes(
    cfg: &config::WsConfig,
    site: &Site,
    root: &std::path::Path,
    sanitized: &str,
    repo: &str,
    event: &str,
) -> bool {
    let routes = inject::routes(cfg, root, sanitized, repo);
    if routes.is_empty() {
        return true;
    }
    if event == "down" {
        for r in &routes {
            crate::caddy::route_down(site, &r.id);
        }
        return true;
    }
    let mut hosts = Vec::new();
    let mut err = None;
    for r in &routes {
        match crate::caddy::route_up(site, r) {
            // Surface the wildcard in the printed host list (`*.<host>`).
            Ok(()) => hosts.push(if r.wildcard { format!("*.{}", r.hostname) } else { r.hostname.clone() }),
            Err(e) => err = Some(format!("{e}")),
        }
    }
    let step = crate::ui::step(repo, "routed");
    match err {
        None => {
            step.ok_with(&hosts.join(", "));
            true
        }
        Some(e) => {
            step.fail(&e);
            false
        }
    }
}

/// Forward a remote repo's allocated service ports back to this machine (`up`) or
/// tear the forwards down (`down`), so each server-bound port is reachable at the
/// same `localhost:<port>` it uses on the server. No-op for a local workspace (the
/// ports are already local). A pure-HTTP container service has no allocated port so
/// it's skipped — it's reached by hostname through Caddy instead.
///
/// Best-effort: a forward failure is noted, never fatal — the service is up on the
/// server regardless, and re-running `up` retries. Generic on purpose: it forwards
/// whatever ports a repo allocated, with no knowledge of what binds them.
///
/// Reaping depends on the `.ws-meta` port record still existing (it's re-derived
/// via `ports::load`); teardown terminates before `ports::forget` wipes it. A
/// record deleted out of band would strand its forwards — acceptable, and the same
/// window `ports::forget` itself has.
fn sync_forwards(
    cfg: &config::WsConfig,
    root: &std::path::Path,
    branch: &str,
    sanitized: &str,
    repo: &str,
    event: &str,
) {
    let Some(r) = cfg.remote.as_ref() else { return };
    for (svc, port) in crate::ports::load(root, sanitized, repo) {
        let session = crate::mutagen::forward_session_name(branch, repo, port);
        if event == "down" {
            let _ = crate::mutagen::forward_terminate(&session);
        } else {
            match crate::mutagen::forward_create(&session, &r.connection, port) {
                Ok(()) => crate::ui::note(&format!("{svc}: reachable at localhost:{port}")),
                Err(e) => crate::ui::note(&format!("{svc}: forward localhost:{port} failed: {e}")),
            }
        }
    }
}

/// Handle one host repo in a sweep. `up` starts its declared `up` script as a
/// detached process with the injected env; `down` stops it.
#[allow(clippy::too_many_arguments)]
fn sweep_host(
    cfg: &config::WsConfig,
    site: &Site,
    root: &std::path::Path,
    wt: &std::path::Path,
    branch: &str,
    sanitized: &str,
    repo: &str,
    event: &str,
    phase: inject::Phase,
    result: &mut SweepResult,
) {
    // Host-process supervision is a detached `ws`-owned process (PID-file tracked)
    // — on the local box, or on the server over SSH for a remote workspace (the
    // `Site` carries the location, so it's the same code path either way).
    let name = crate::supervise::proc_name(branch, repo);

    if event == "down" {
        let step = crate::ui::step(repo, "stop");
        result.record(repo, step, crate::supervise::down(site, &name));
        if cfg.routing_active() {
            // Best-effort for a host repo (unlike a container): a host service
            // binds a real `WS_PORT` on the host, so it stays reachable there even
            // if its Caddy route (the hostname) fails to sync.
            let _ = sync_routes(cfg, site, root, sanitized, repo, "down");
        }
        sync_forwards(cfg, root, branch, sanitized, repo, "down");
        return;
    }

    // `up`: the launch command is the repo's declared `up` script.
    let Some(cmd) = cfg.script_command(repo, "up") else {
        crate::ui::skip(repo, "no up script");
        result.skipped.push(repo.to_string());
        return;
    };

    let extra_env = match inject::prepare(cfg, site, root, wt, sanitized, repo, phase) {
        Ok(env) => env,
        Err(e) => {
            crate::ui::step(repo, "inject").fail(&format!("{e}"));
            result.failed.push(repo.to_string());
            return;
        }
    };

    // The process's working dir at its execution site: the server-side synced dir
    // (absolute) for a remote workspace — flush first so the launched process sees
    // the rendered templates / latest code — else the local worktree.
    let worktree = match cfg.remote.as_ref() {
        Some(r) => {
            let _ = crate::mutagen::flush(&crate::mutagen::session_name(branch, repo));
            let base = site.abs_base(&r.worktree_base(root));
            // Ship `.ws-hooks/` so a host `up` referencing `$WS_WORKSPACE_ROOT/
            // .ws-hooks/…` resolves on the server (same lane the container path uses).
            if event == "up" {
                if let Err(e) = crate::remote::push_ws_hooks(site, root, &base) {
                    crate::ui::step(repo, &format!("{event} host")).fail(&format!("{e}"));
                    result.failed.push(repo.to_string());
                    return;
                }
            }
            crate::remote::worktree_path(&base, sanitized, repo)
        }
        None => wt.to_string_lossy().into_owned(),
    };

    // The host process runs detached (not via the `Command`-based hook runner), so
    // apply the base `WS_*` contract here — otherwise the `up` script sees only the
    // injection env (`WS_PORT_*`/`WS_HOST_*`) and `$WS_BRANCH_SANITIZED` & friends
    // come out empty (e.g. `https://.api.<domain>`).
    let mut proc_env = crate::hooks::base_env(root, &worktree, repo, sanitized, event);
    proc_env.extend(extra_env.iter().cloned());
    match crate::supervise::up(site, &name, &worktree, &cmd, &proc_env) {
        Ok(started) => {
            let label = if started { "start" } else { "already running (ws down to apply changes)" };
            // Surface the ports ws allocated for this host process. Unlike a routed
            // container (which prints its hostname), a host service has no hostname
            // to show — the bound port is the only handle, and it's otherwise
            // invisible to the user. Sorted so the line is stable across runs.
            let mut ports: Vec<String> = extra_env
                .iter()
                .filter_map(|(k, v)| k.strip_prefix("WS_PORT_").map(|svc| format!("{svc}:{v}")))
                .collect();
            ports.sort();
            let step = crate::ui::step(repo, label);
            if ports.is_empty() {
                step.ok();
            } else {
                step.ok_with(&ports.join(", "));
            }
            result.ran.push(repo.to_string());
            if cfg.routing_active() {
                // Best-effort (see the `down` arm): a host service keeps its
                // bound `WS_PORT`, so a route failure doesn't strand it.
                let _ = sync_routes(cfg, site, root, sanitized, repo, "up");
            }
            // Make the server-bound ports reachable at localhost on this machine.
            sync_forwards(cfg, root, branch, sanitized, repo, "up");
        }
        Err(e) => {
            crate::ui::step(repo, "start").fail(&format!("{e}"));
            result.failed.push(repo.to_string());
        }
    }
}
