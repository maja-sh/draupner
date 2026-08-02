//! Shared driver for `ws up` and `ws down` — the symmetric hook-runner pair.
//!
//! Both ops do the same thing: for every worktree of a branch, run one hook
//! event (`up` or `down`), isolating per-repo failures so one repo's broken
//! hook doesn't strand the others. They differ only in the event name, header,
//! and closing summary — those stay in the op modules.

use anyhow::Result;

use crate::config;
use crate::inject;
use crate::remote::Site;
use crate::workspace;

/// How long a freshly launched host process gets to bind the port ws allocated
/// before `ws up` reports that it hasn't. Long enough for an interpreted dev
/// server (vite, `artisan serve`) to be listening, short enough that a compiled
/// one — which will take far longer than any bearable wait — doesn't hold up the
/// op. Missing the window costs a qualified line, not a failure.
const BIND_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

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

    /// Compute a repo's injection context (ports/hosts/env) and render its
    /// templates, filing the repo under `failed` — with the `inject` step line —
    /// when it errors, so the caller can just skip the repo. A failure here is
    /// isolated per-repo, like a failing hook: it doesn't strand the other repos
    /// in the sweep.
    #[allow(clippy::too_many_arguments)]
    fn inject_env(
        &mut self,
        cfg: &config::WsConfig,
        site: &Site,
        root: &std::path::Path,
        wt: &std::path::Path,
        sanitized: &str,
        repo: &str,
        phase: inject::Phase,
    ) -> Option<Vec<(String, String)>> {
        match inject::prepare(cfg, site, root, wt, sanitized, repo, phase) {
            Ok(env) => Some(env),
            Err(e) => {
                crate::ui::step(repo, "inject").fail(&format!("{e}"));
                self.failed.push(repo.to_string());
                None
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
    // unreachable). `ensure_routing` owns the Caddy-side sequencing and the DNS
    // verdict; we only surface what it returns.
    if event == "up" {
        if let Some(domain) = cfg.domain.as_deref().filter(|_| cfg.routing_active()) {
            // `ensure_routing` already probes DNS, and its `Dns` *is* the verdict —
            // `resolve_hint` only phrases it. Reuse it so the auto-offer below
            // doesn't resolve DNS a second time.
            let configured = cfg.remote.as_ref().and_then(|r| r.publish_address.as_deref());
            let publish = crate::caddy::publish_address(&site, configured);
            // Say it out loud. The default is right (offload is pointless if the
            // client can't reach it), but on a box with a public interface it also
            // serves every worktree to the internet, and nothing else here would
            // tell you that.
            if site.is_remote() && configured.is_none() {
                // Name the subject and the consequence. The old wording led with a
                // bare verb ("publishing on all interfaces") that never said what
                // was publishing or what it cost, so it read as a config nag rather
                // than the exposure notice it is.
                crate::ui::note(
                    "server Caddy binds every interface — worktree HTTP is exposed on all of \
                     the server's networks; pin one with `remote.publish_address` in .ws.json",
                );
            }
            let dns = match crate::caddy::ensure_routing(&site, cfg.runtime(), domain, &publish) {
                Ok(state) => {
                    if let Some(h) = crate::caddy::resolve_hint(&site, domain, &state) {
                        crate::ui::note(&h);
                    }
                    state
                }
                Err(e) => {
                    // Caddy never came up, so nothing was resolved. Treat DNS as
                    // unwired rather than inventing a verdict — the nudge below is
                    // the right outcome either way.
                    crate::ui::note(&format!("routing infra not ready: {e:#}"));
                    crate::caddy::Dns::Unresolved
                }
            };
            // Auto-offer setup when a privileged bit (DNS / CA trust / :443) is
            // missing — a non-mutating nudge to run `ws setup`. Skipped under an
            // externally-managed Caddy (BYO / the test seam).
            //
            // `ws setup` handles a remote workspace too (the client's DNS + the
            // server's CA), but the full probe isn't worth its cost on the remote
            // up path: it would need the server address resolved and its CA check
            // dials the server. The `dns` verdict is already in hand for free and is
            // the bit that usually isn't wired, so remote nudges on that alone.
            if !crate::caddy::externally_managed() {
                let incomplete = if site.is_remote() {
                    !dns.is_ok()
                } else {
                    crate::setup::probe(&site, cfg.runtime(), domain, "127.0.0.1", Some(dns))
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
        if event == "up" && !run_prepare(&cfg, &ws.root, wt, branch, &sanitized, repo, &mut result) {
            continue;
        }

        // Host processes are supervised as a detached `ws`-owned process, not run as a
        // blocking hook — `up` launches it (detached, survives `ws` exiting), `down`
        // stops. Their `up` script binds the injected `WS_PORT_*`.
        if cfg.is_host_repo(repo) {
            sweep_host(&cfg, &site, &ws.root, wt, branch, &sanitized, repo, event, phase, &mut result);
            continue;
        }

        // The container lifecycle runs where the stack lives — streamed here for a
        // local workspace, over SSH in the synced worktree for a remote one. One
        // dispatcher answers that (`lifecycle::Ctx`), so everything below is the
        // same code either way.
        let ctx = crate::lifecycle::Ctx {
            cfg: &cfg, site: &site, root: &ws.root, worktree: wt,
            branch, sanitized: &sanitized, repo,
        };

        let cmd = match ctx.command(event) {
            Ok(Some(cmd)) => cmd,
            Ok(None) => {
                crate::ui::skip(repo, &format!("no {event} hook"));
                result.skipped.push(repo.clone());
                continue;
            }
            Err(e) => {
                crate::ui::step(repo, &format!("{event} hook")).fail(&format!("{e}"));
                result.failed.push(repo.clone());
                continue;
            }
        };

        // On a remote workspace's `up`, `prepare` derives services + writes the
        // override by reading the worktree *on the server* — so ship our latest
        // edits before that, else a just-edited compose file would derive stale.
        // (The dispatch below flushes again, cheaply; `down`/Inspect doesn't derive
        // but the flush is harmless. Local is a no-op.)
        ctx.flush_to_server();

        let Some(extra_env) = result.inject_env(&cfg, &site, &ws.root, wt, &sanitized, repo, phase)
        else {
            continue;
        };

        let step = crate::ui::step(repo, &format!("{event} hook"));
        let res = ctx.run(event, &cmd, &extra_env);
        let up_ok = res.is_ok();
        result.record(repo, step, res);

        route_after_hook(&cfg, &site, &ws.root, &sanitized, repo, event, up_ok, &mut result);
        // Forward this repo's raw-TCP ports (db, redis, …) to localhost, so a DB GUI
        // or CLI on this machine reaches the server's containers unchanged. A no-op
        // for a local workspace — the ports are already local.
        if event == "down" || up_ok {
            sync_forwards(&cfg, &ws.root, branch, &sanitized, repo, event);
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
    branch: &str,
    sanitized: &str,
    repo: &str,
    result: &mut SweepResult,
) -> bool {
    // `Site::Local` is the whole point here — the same dispatcher every other
    // event uses, told to stay on this machine.
    let ctx = crate::lifecycle::Ctx {
        cfg, site: &Site::Local, root, worktree: wt, branch, sanitized, repo,
    };
    let cmd = match ctx.command("prepare") {
        Ok(Some(cmd)) => cmd,
        Ok(None) => return true,
        Err(e) => {
            crate::ui::step(repo, "prepare hook").fail(&format!("{e}"));
            result.failed.push(repo.to_string());
            return false;
        }
    };
    let extra_env =
        inject::prepare(cfg, &Site::Local, root, wt, sanitized, repo, inject::Phase::Inspect)
            .unwrap_or_default();
    let step = crate::ui::step(repo, "prepare hook");
    match ctx.run("prepare", &cmd, &extra_env) {
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

/// Sync a repo's Caddy routes once its hook has run: add after a successful `up`,
/// remove on `down`. A container's HTTP services are reachable ONLY through Caddy
/// now (the override `!reset`s their host publish), so an `up` route that won't
/// install means the running container is unreachable — demote the repo into
/// `failed`.
///
/// The hook paths only. A host repo routes best-effort instead (see `sweep_host`):
/// it binds a real `WS_PORT` on the host, so it stays reachable there even if its
/// Caddy route (the hostname) fails to sync.
#[allow(clippy::too_many_arguments)]
fn route_after_hook(
    cfg: &config::WsConfig,
    site: &Site,
    root: &std::path::Path,
    sanitized: &str,
    repo: &str,
    event: &str,
    up_ok: bool,
    result: &mut SweepResult,
) {
    if !cfg.routing_active() || !(event == "down" || up_ok) {
        return;
    }
    if !sync_routes(cfg, site, root, sanitized, repo, event) && event == "up" {
        result.demote(repo);
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
    // Successes become one aligned block. A stack with a dozen services produced a
    // dozen near-identical notes that buried everything else in the sweep, and the
    // interesting part of each was two words wide. Failures keep their own line:
    // they're rare, individually actionable, and carry an error to read.
    let mut forwarded: Vec<(String, String)> = Vec::new();
    for (svc, port) in crate::ports::load(root, sanitized, repo) {
        let session = crate::mutagen::forward_session_name(branch, repo, port);
        if event == "down" {
            let _ = crate::mutagen::forward_terminate(&session);
        } else {
            match crate::mutagen::forward_create(&session, &r.connection, port) {
                // The value is the full dial string, not a bare port, so it can be
                // pasted straight into a client.
                Ok(()) => forwarded.push((svc, format!("localhost:{port}"))),
                Err(e) => crate::ui::note(&format!("{svc}: forward localhost:{port} failed: {e}")),
            }
        }
    }
    crate::ui::table(repo, "port forwards", &forwarded);
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

    let Some(extra_env) = result.inject_env(cfg, site, root, wt, sanitized, repo, phase) else {
        return;
    };

    // The process's working dir and workspace root at its execution site: the
    // server-side synced dirs (absolute) for a remote workspace, else the local
    // ones. Both come from `Site`, which owns the derivation and the reason the two
    // must agree: `WS_WORKSPACE_ROOT` is what a `.ws-hooks/…` script path anchors
    // against, so handing back the local root while the process runs on the server
    // points it at a filesystem that isn't there (`/Users/…: not found` on a Linux
    // box).
    let site_root = site.root_at(cfg.remote.as_ref(), root);
    let worktree = site.worktree_at(cfg.remote.as_ref(), root, wt, sanitized, repo);

    // Flush so the launched process sees the rendered templates / latest code
    // rather than whatever the last sync happened to carry, and ship `.ws-hooks/`
    // so a host `up` referencing `$WS_WORKSPACE_ROOT/.ws-hooks/…` resolves on the
    // server. Literally the same staging the dispatched lifecycle events get —
    // borrowed from the dispatcher rather than spelled out again — and a no-op for
    // a local workspace. (The root it returns is `site_root`, already in hand.)
    let ctx = crate::lifecycle::Ctx { cfg, site, root, worktree: wt, branch, sanitized, repo };
    if let Err(e) = ctx.push_to_server() {
        crate::ui::step(repo, &format!("{event} host")).fail(&format!("{e}"));
        result.failed.push(repo.to_string());
        return;
    }

    // The host process runs detached (not via the `Command`-based hook runner), so
    // apply the base `WS_*` contract here — otherwise the `up` script sees only the
    // injection env (`WS_PORT_*`/`WS_HOST_*`) and `$WS_BRANCH_SANITIZED` & friends
    // come out empty (e.g. `https://.api.<domain>`).
    //
    // Third time this detached path was caught missing something the dispatched
    // lifecycle already did — after the base `WS_*` contract itself, and the
    // harvested login PATH. Two of those three now have a single owner each: the
    // staging above is `lifecycle::Ctx::push_to_server`, the PATH is
    // `remote::harvested_path` (applied inside `supervise::up`). This env assembly
    // is the piece still written twice, because a detached process isn't a
    // dispatched command — anything added to `hooks::base_env`'s contract still has
    // to reach both.
    let mut proc_env =
        crate::hooks::base_env(std::path::Path::new(&site_root), &worktree, repo, sanitized, event);
    proc_env.extend(extra_env.iter().cloned());
    match crate::supervise::up(site, &name, &worktree, &cmd, &proc_env) {
        Ok(started) => {
            let label = if started { "start" } else { "already running (ws down to apply changes)" };
            let (ports, allocated) = host_ports(&extra_env);
            let step = crate::ui::step(repo, label);

            // The post-condition. `supervise::up` proves a process was launched and
            // survived half a second; it does not prove the thing that launch was
            // *for* — that the port ws allocated is now being served by it. Those
            // came apart repeatedly: a server that bound a different port after
            // finding this one busy, an orphan from an earlier run still holding it.
            // Both left `start web:20001` on screen and the failure surfacing much
            // later somewhere else.
            let health = crate::supervise::health_settled(site, &name, &allocated, BIND_GRACE);
            if report_host_health(step, repo, &name, &ports, health) {
                result.failed.push(repo.to_string());
                return;
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

/// Split a host repo's injected `WS_PORT_*` entries into the ports to display and
/// the ports to probe.
///
/// The display list surfaces the ports ws allocated for this host process. Unlike
/// a routed container (which prints its hostname), a host service has no hostname
/// to show — the bound port is the only handle, and it's otherwise invisible to
/// the user. Sorted so the line is stable across runs.
///
/// The two lists are deliberately not the same set: an entry whose value isn't a
/// port number still shows on the line, but there's nothing there to probe.
fn host_ports(extra_env: &[(String, String)]) -> (Vec<String>, Vec<u16>) {
    let mut ports = Vec::new();
    let mut allocated = Vec::new();
    for (k, v) in extra_env {
        let Some(svc) = k.strip_prefix("WS_PORT_") else { continue };
        ports.push(format!("{svc}:{v}"));
        if let Ok(port) = v.parse() {
            allocated.push(port);
        }
    }
    ports.sort();
    (ports, allocated)
}

/// Turn a launched host process's settled health into its step line (and any
/// follow-up note/warning). Returns whether it crashed after launch.
fn report_host_health(
    step: crate::ui::Step,
    repo: &str,
    name: &str,
    ports: &[String],
    health: Option<crate::supervise::Health>,
) -> bool {
    let crashed = health == Some(crate::supervise::Health::Crashed);

    match &health {
        // Dying *after* the launch check is a failed `up`, not a green one
        // with a caveat — there is nothing running left to talk to.
        Some(crate::supervise::Health::Crashed) => {
            step.fail(&format!("exited after launch — see {}", crate::supervise::log_hint(name)));
        }
        // Alive but nothing on the port yet is genuinely ambiguous — a
        // `cargo watch` compiles for a minute before it listens — so it
        // qualifies the claim rather than failing the repo.
        Some(crate::supervise::Health::Silent(_)) => {
            step.ok_with(&format!("{} — not listening yet", ports.join(", ")));
            crate::ui::note(&format!(
                "{repo}: nothing is listening on the port ws allocated yet. If it's still \
                 starting this resolves itself — `ws list` will say. Otherwise check \
                 `{log}`.",
                log = crate::supervise::log_hint(name),
            ));
        }
        // A stranger on the port is not ambiguous: whatever just started
        // cannot have it, so the server is either dead or somewhere else.
        Some(crate::supervise::Health::Squatted(p)) => {
            step.ok_with(&format!("{} — port held by another process", ports.join(", ")));
            crate::ui::warn(
                &format!("{repo}: another process already holds the allocated port"),
                &p.iter()
                    .map(|port| {
                        format!(
                            ":{port} is held by something outside this process — a dev \
                             server that finds it busy will quietly move to another port"
                        )
                    })
                    .collect::<Vec<_>>(),
            );
        }
        _ if ports.is_empty() => step.ok(),
        _ => step.ok_with(&ports.join(", ")),
    }

    crashed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn host_ports_displays_sorted_and_probes_only_the_numeric_ones() {
        let (ports, allocated) = host_ports(&env(&[
            ("WS_PORT_web", "20001"),
            ("WS_BRANCH", "test/foo"),
            ("WS_PORT_api", "20000"),
            ("WS_HOST_web", "web.test-foo.example"),
        ]));
        assert_eq!(ports, vec!["api:20000".to_string(), "web:20001".to_string()]);
        // Probe order follows the env, not the display sort.
        assert_eq!(allocated, vec![20001, 20000]);
    }

    #[test]
    fn host_ports_shows_an_unparsable_port_but_does_not_probe_it() {
        let (ports, allocated) = host_ports(&env(&[("WS_PORT_web", "not-a-port")]));
        assert_eq!(ports, vec!["web:not-a-port".to_string()]);
        assert!(allocated.is_empty());
    }

    #[test]
    fn host_ports_is_empty_without_injected_ports() {
        let (ports, allocated) = host_ports(&env(&[("WS_REPO", "api")]));
        assert!(ports.is_empty());
        assert!(allocated.is_empty());
    }

    #[test]
    fn report_host_health_reports_a_crash_and_nothing_else() {
        assert!(report_host_health(
            crate::ui::step("api", "start"),
            "api",
            "ws-test-foo-api",
            &["web:20001".to_string()],
            Some(crate::supervise::Health::Crashed),
        ));
        for health in [
            None,
            Some(crate::supervise::Health::Serving(vec![20001])),
            Some(crate::supervise::Health::Running),
            Some(crate::supervise::Health::NotStarted),
            Some(crate::supervise::Health::Silent(vec![20001])),
            Some(crate::supervise::Health::Squatted(vec![20001])),
        ] {
            assert!(!report_host_health(
                crate::ui::step("api", "start"),
                "api",
                "ws-test-foo-api",
                &["web:20001".to_string()],
                health,
            ));
        }
    }

    #[test]
    fn demote_moves_a_ran_repo_into_failed_exactly_once() {
        let mut result =
            SweepResult { ran: vec!["api".into()], skipped: Vec::new(), failed: Vec::new() };
        result.demote("api");
        result.demote("api");
        assert!(result.ran.is_empty());
        assert_eq!(result.failed, vec!["api".to_string()]);
    }
}
