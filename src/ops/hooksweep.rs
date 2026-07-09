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
            let dns_ok = match crate::caddy::ensure_routing(cfg.runtime(), domain) {
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
            // externally-managed Caddy (BYO / the test seam): there's no real
            // infra to probe and nothing for the user to set up.
            if !crate::caddy::externally_managed()
                && crate::setup::probe(cfg.runtime(), domain, Some(dns_ok)).privileged_gaps()
            {
                crate::ui::nudge("`ws setup` — some routing setup (DNS / CA trust) is incomplete");
            }
        }
    }

    for (repo, wt) in &targets {
        super::bail_if_interrupted()?;

        // Host processes are supervised as `systemd --user` units, not run as a
        // blocking hook — `up` launches (detached, crash-restarting), `down`
        // stops. Their `up` script binds the injected `WS_PORT_*`.
        if cfg.is_host_repo(repo) {
            sweep_host(&cfg, &ws.root, wt, branch, &sanitized, repo, event, phase, &mut result);
            continue;
        }

        let Some(action) = hooks::resolve(&cfg, &ws.root, repo, event) else {
            crate::ui::skip(repo, &format!("no {event} hook"));
            result.skipped.push(repo.clone());
            continue;
        };

        // Compute the injection context (ports/hosts/env) and render templates.
        // A failure here is isolated per-repo, like a failing hook — it doesn't
        // strand the other repos in the sweep.
        let extra_env = match inject::prepare(&cfg, &ws.root, wt, &sanitized, repo, phase) {
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
            let routed_ok = sync_routes(&cfg, &ws.root, &sanitized, repo, event);
            if event == "up" && !routed_ok {
                result.demote(repo);
            }
        }
    }

    Ok(result)
}

/// Add (`up`) or remove (`down`) the Caddy routes for a repo's HTTP services.
/// Returns whether the sync succeeded: always `true` for `down` (removal is
/// best-effort — teardown is infallible by design) and for a repo with no HTTP
/// routes; on `up`, `false` if any route failed to install (the container caller
/// fails the repo, since an HTTP container has no host-port fallback).
fn sync_routes(
    cfg: &config::WsConfig,
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
            crate::caddy::route_down(&r.id);
        }
        return true;
    }
    let mut hosts = Vec::new();
    let mut err = None;
    for r in &routes {
        match crate::caddy::route_up(&r.id, &r.hostname, &r.upstream) {
            Ok(()) => hosts.push(r.hostname.clone()),
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

/// Handle one host repo in a sweep. `up` starts its declared `up` script as a
/// transient `systemd --user` unit with the injected env; `down` stops the unit.
#[allow(clippy::too_many_arguments)]
fn sweep_host(
    cfg: &config::WsConfig,
    root: &std::path::Path,
    wt: &std::path::Path,
    branch: &str,
    sanitized: &str,
    repo: &str,
    event: &str,
    phase: inject::Phase,
    result: &mut SweepResult,
) {
    let unit = crate::supervise::unit_name(branch, repo);

    if event == "down" {
        let step = crate::ui::step(repo, "stop (systemd)");
        result.record(repo, step, crate::supervise::down(&unit));
        if cfg.routing_active() {
            // Best-effort for a host repo (unlike a container): a host service
            // binds a real `WS_PORT` on the host, so it stays reachable there even
            // if its Caddy route (the hostname) fails to sync.
            let _ = sync_routes(cfg, root, sanitized, repo, "down");
        }
        return;
    }

    // `up`: the launch command is the repo's declared `up` script.
    let Some(cmd) = cfg
        .repo_config(repo)
        .and_then(|rc| rc.scripts.get("up"))
        .map(|s| s.commands().join(" && "))
    else {
        crate::ui::skip(repo, "no up script");
        result.skipped.push(repo.to_string());
        return;
    };

    let extra_env = match inject::prepare(cfg, root, wt, sanitized, repo, phase) {
        Ok(env) => env,
        Err(e) => {
            crate::ui::step(repo, "inject").fail(&format!("{e}"));
            result.failed.push(repo.to_string());
            return;
        }
    };

    match crate::supervise::up(&unit, wt, &cmd, &extra_env) {
        Ok(started) => {
            let label = if started { "start (systemd)" } else { "already running (ws down to apply changes)" };
            crate::ui::step(repo, label).ok();
            result.ran.push(repo.to_string());
            if cfg.routing_active() {
                // Best-effort (see the `down` arm): a host service keeps its
                // bound `WS_PORT`, so a route failure doesn't strand it.
                let _ = sync_routes(cfg, root, sanitized, repo, "up");
            }
        }
        Err(e) => {
            crate::ui::step(repo, "start (systemd)").fail(&format!("{e}"));
            result.failed.push(repo.to_string());
        }
    }
}
