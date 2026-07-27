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
            // externally-managed Caddy (BYO / the test seam) and for a remote
            // workspace: `ws setup` is host-role and its probe reports the *host's*
            // DNS/CA, not the server's (remote setup is a later slice).
            if !site.is_remote()
                && !crate::caddy::externally_managed()
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

        let Some(action) = hooks::resolve(&cfg, &ws.root, repo, event) else {
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
    let Some(cmd) = cfg.script_command(repo, event) else {
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
    let res = super::run_remote_event(r, root, branch, sanitized, repo, event, &cmd, &extra_env);
    let up_ok = res.is_ok();
    result.record(repo, step, res);

    if cfg.routing_active() && (event == "down" || up_ok) {
        let routed_ok = sync_routes(cfg, site, root, sanitized, repo, event);
        if event == "up" && !routed_ok {
            result.demote(repo);
        }
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

/// Handle one host repo in a sweep. `up` starts its declared `up` script as a
/// transient `systemd --user` unit with the injected env; `down` stops the unit.
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
    // Host-process supervision is a `systemd --user` transient unit — on the local
    // box, or on the server over SSH for a remote workspace (the `Site` carries
    // the location; `supervise` sets `XDG_RUNTIME_DIR` remotely so `systemctl
    // --user` finds the user bus).
    let unit = crate::supervise::unit_name(branch, repo);

    if event == "down" {
        let step = crate::ui::step(repo, "stop (systemd)");
        result.record(repo, step, crate::supervise::down(site, &unit));
        if cfg.routing_active() {
            // Best-effort for a host repo (unlike a container): a host service
            // binds a real `WS_PORT` on the host, so it stays reachable there even
            // if its Caddy route (the hostname) fails to sync.
            let _ = sync_routes(cfg, site, root, sanitized, repo, "down");
        }
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
            crate::remote::worktree_path(&site.abs_base(&r.worktree_base(root)), sanitized, repo)
        }
        None => wt.to_string_lossy().into_owned(),
    };

    match crate::supervise::up(site, &unit, &worktree, &cmd, &extra_env) {
        Ok(started) => {
            let label = if started { "start (systemd)" } else { "already running (ws down to apply changes)" };
            crate::ui::step(repo, label).ok();
            result.ran.push(repo.to_string());
            if cfg.routing_active() {
                // Best-effort (see the `down` arm): a host service keeps its
                // bound `WS_PORT`, so a route failure doesn't strand it.
                let _ = sync_routes(cfg, site, root, sanitized, repo, "up");
            }
        }
        Err(e) => {
            crate::ui::step(repo, "start (systemd)").fail(&format!("{e}"));
            result.failed.push(repo.to_string());
        }
    }
}
