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

    // Bring up the shared routing infra once before the loop, fatally: since the
    // override `!reset`s HTTP host-ports, Caddy is the only path to an HTTP
    // service, so a stack brought "up" without it would be a lie. `ensure_routing`
    // owns the Caddy-side sequencing (create `ws-net`, start Caddy) + the
    // DNS-not-ready hint; we decide fatality and surface the hint it returns.
    //
    // KNOWN LIMITATION: `routing_active()` is workspace-wide, so this is fatal
    // even for a sweep whose repos are all raw-TCP (no HTTP service ⇒ no route,
    // no `ws-net` join) and thus need no Caddy. Niche — a `domain` is set because
    // some repo routes — and the alternative (deriving every repo's services here
    // just to decide) isn't worth it. Documented in HANDOFF.
    if event == "up" {
        if let Some(domain) = cfg.domain.as_deref().filter(|_| cfg.routing_active()) {
            match crate::caddy::ensure_routing(cfg.runtime(), domain) {
                Ok(Some(hint)) => crate::ui::note(&hint),
                Ok(None) => {}
                Err(e) => return Err(super::op_failure(format!("{e:#}"))),
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

        // Sync Caddy routes: add after a successful `up`, remove on `down`.
        // Best-effort per repo — Caddy itself is already confirmed up (the fatal
        // `ensure_routing` above), so a route failure here is a rare transient
        // admin error, surfaced on its own step line and recoverable by re-running
        // `ws up`; the containers are healthy either way.
        if cfg.routing_active() && (event == "down" || up_ok) {
            sync_routes(&cfg, &ws.root, &sanitized, repo, event);
        }
    }

    Ok(result)
}

/// Add (`up`) or remove (`down`) the Caddy routes for a repo's HTTP services.
/// Best-effort per repo: a route-sync failure is surfaced on its own step line
/// but doesn't fail the op. Caddy itself is already confirmed up (the fatal
/// `ensure_routing` above), so a failure here is a rare per-route admin error,
/// recoverable by re-running `ws up`; the containers are healthy either way.
fn sync_routes(
    cfg: &config::WsConfig,
    root: &std::path::Path,
    sanitized: &str,
    repo: &str,
    event: &str,
) {
    let routes = inject::routes(cfg, root, sanitized, repo);
    if routes.is_empty() {
        return;
    }
    if event == "down" {
        for r in &routes {
            crate::caddy::route_down(&r.id);
        }
        return;
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
        None => step.ok_with(&hosts.join(", ")),
        Some(e) => step.fail(&e),
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
            sync_routes(cfg, root, sanitized, repo, "down");
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
                sync_routes(cfg, root, sanitized, repo, "up");
            }
        }
        Err(e) => {
            crate::ui::step(repo, "start (systemd)").fail(&format!("{e}"));
            result.failed.push(repo.to_string());
        }
    }
}
