//! Shared driver for `drup up` and `drup down` — run one hook event across every worktree of a branch, isolating per-repo failures.

use anyhow::Result;

use crate::config;
use crate::hooks;
use crate::inject;
use crate::remote::Site;
use crate::workspace;

/// Window for a launched host process to bind its allocated port before `drup up` qualifies the line — long enough for an interpreted dev server, short enough not to stall on a compiled one. Missing it costs a note, not a failure.
const BIND_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// Outcome of a sweep, for the caller to render its own summary from.
pub struct SweepResult {
    pub ran: Vec<String>,
    pub skipped: Vec<String>,
    /// Non-empty ⇒ the op should exit 1.
    pub failed: Vec<String>,
    /// Interactive hooks refused for lack of a controlling terminal — distinct
    /// from `failed` so the caller can surface the "re-run from your shell"
    /// nudge and exit with the dedicated code 3.
    pub needs_human: Vec<String>,
}

impl SweepResult {
    fn record(&mut self, repo: &str, step: crate::ui::Step, res: Result<()>) {
        match res {
            Ok(()) => {
                step.ok();
                self.ran.push(repo.to_string());
            }
            Err(e) => {
                step.fail(&format!("{e}"));
                self.file_failure(repo, &e);
            }
        }
    }

    /// Route a hook error to `needs_human` iff it carries the interactive-refused
    /// marker, else `failed`. Shared between `record` and the `prepare` path.
    fn file_failure(&mut self, repo: &str, err: &anyhow::Error) {
        let bucket = if format!("{err}").contains(hooks::INTERACTIVE_REFUSED_PREFIX) {
            &mut self.needs_human
        } else {
            &mut self.failed
        };
        bucket.push(repo.to_string());
    }

    /// Move `repo` from `ran` to `failed` — the hook succeeded but a follow-up (Caddy route) left the stack unreachable, since the host publish is `!reset` away.
    fn demote(&mut self, repo: &str) {
        self.ran.retain(|r| r != repo);
        if !self.failed.iter().any(|r| r == repo) {
            self.failed.push(repo.to_string());
        }
    }
}

/// Run `event`'s hook across every worktree of `branch`; missing hooks skip, failing hooks are reported but don't stop the remaining repos. Preflight-fails (exit 2) when the branch has no worktrees.
pub fn sweep(branch: &str, event: &str, op: &str) -> Result<SweepResult> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;
    let sanitized = workspace::sanitize_branch(branch);

    let targets = workspace::task_worktrees(&ws.root, &cfg, branch);

    if targets.is_empty() {
        return Err(super::preflight(format!(
            "no worktrees for {branch}; run `drup new` first"
        )));
    }

    // Resolving a remote site fetches the server `$HOME`, so this doubles as
    // reachability probe. `down` degrades to local-only cleanup when the
    // server is out of reach — the local mutagen forwards would otherwise
    // leak (`drup rm` already handles this the same way). Every other event
    // still hard-fails: `up` without a reachable site would silently skip
    // half its work.
    let site = match Site::resolve(&cfg) {
        Ok(s) => s,
        Err(e) if event == "down" => {
            let conn = cfg.remote.as_ref().map(|r| r.connection.as_str()).unwrap_or("remote");
            crate::ui::header(op, branch);
            crate::ui::note(&format!(
                "{conn}: server unreachable ({e:#}); tearing down local forwards only, \
                 server-side stacks and routes are left as-is"
            ));
            let mut result = SweepResult { ran: Vec::new(), skipped: Vec::new(), failed: Vec::new(), needs_human: Vec::new() };
            for (repo, _wt) in &targets {
                for (_, port) in crate::ports::load(&ws.root, &sanitized, repo) {
                    let _ = crate::mutagen::forward_terminate(
                        &crate::mutagen::forward_session_name(branch, repo, port),
                    );
                }
                result.skipped.push(repo.clone());
            }
            return Ok(result);
        }
        Err(e) => return Err(e),
    };

    crate::ui::header(op, branch);

    let mut result = SweepResult { ran: Vec::new(), skipped: Vec::new(), failed: Vec::new(), needs_human: Vec::new() };

    // `up` allocates; `down` only reads what's already allocated.
    let phase = if event == "up" { inject::Phase::Activate } else { inject::Phase::Inspect };

    // Best-effort: a sweep of only raw-TCP repos needs neither Caddy nor `drup-net`, so a broken infra shouldn't block it; failure surfaces later where routing is load-bearing (HTTP container's own `compose up`, or the demote below).
    if event == "up" {
        if let Some(domain) = cfg.domain.as_deref().filter(|_| cfg.routing_active()) {
            // `ensure_routing` probes DNS; reuse its verdict for the auto-offer below rather than resolving twice.
            let configured = cfg.remote.as_ref().and_then(|r| r.publish_address.as_deref());
            let publish = crate::caddy::publish_address(&site, configured);
            // On a public interface the wildcard bind exposes every worktree to the internet — nothing else here would say so.
            if site.is_remote() && configured.is_none() {
                crate::ui::note(
                    "server Caddy binds every interface — worktree HTTP is exposed on all of \
                     the server's networks; pin one with `remote.publish_address` in .drup.json",
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
                    // Caddy never came up, so nothing was resolved — treat DNS as unwired.
                    crate::ui::note(&format!("routing infra not ready: {e:#}"));
                    crate::caddy::Dns::Unresolved
                }
            };
            // Nudge to run `drup setup` when a privileged bit (DNS/CA/:443) is missing. Remote nudges on DNS alone — the full probe would dial the server.
            if !crate::caddy::externally_managed() {
                let incomplete = if site.is_remote() {
                    !dns.is_ok()
                } else {
                    crate::setup::probe(&site, cfg.runtime(), domain, "127.0.0.1", Some(dns))
                        .privileged_gaps()
                };
                if incomplete {
                    crate::ui::nudge(
                        "`drup setup` — some routing setup (DNS / CA trust) is incomplete",
                    );
                }
            }
        }
    }

    for (repo, wt) in &targets {
        super::bail_if_interrupted()?;

        // `prepare` runs on THIS machine even for a remote workspace — the one hook that doesn't follow the stack — so it can stage client-side data into the worktree for the up-flush to ship. Failed prepare skips this repo's up.
        if event == "up" && !run_prepare(&cfg, &ws.root, wt, &sanitized, repo, &mut result) {
            continue;
        }

        // Host processes are supervised as a detached `drup`-owned process, not run as a blocking hook.
        if cfg.is_host_repo(repo) {
            sweep_host(&cfg, &site, &ws.root, wt, branch, &sanitized, repo, event, phase, &mut result);
            continue;
        }

        if site.is_remote() {
            sweep_container_remote(
                &cfg, &site, &ws.root, wt, branch, &sanitized, repo, event, phase, &mut result,
            );
            continue;
        }

        let script = match hooks::resolve(&cfg, &ws.root, repo, event, cfg.is_container_repo(repo, wt))
        {
            Ok(Some(s)) => s,
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

        // A failure here is isolated per-repo, like a failing hook.
        let extra_env = match inject::prepare(&cfg, &site, &ws.root, wt, &sanitized, repo, phase) {
            Ok(env) => env,
            Err(e) => {
                crate::ui::step(repo, "inject").fail(&format!("{e}"));
                result.failed.push(repo.clone());
                continue;
            }
        };

        let step = crate::ui::step(repo, &format!("{event} hook"));
        let res = hooks::run_command(
            &ws.root, wt, repo, &sanitized, event, &script.cmd, &extra_env, script.interactive,
        );
        let up_ok = res.is_ok();
        result.record(repo, step, res);

        // Container HTTP is reachable ONLY through Caddy (the override `!reset`s the host publish), so an `up` route that won't install means the container is unreachable — fail the repo.
        if cfg.routing_active() && (event == "down" || up_ok) {
            let routed_ok = sync_routes(&cfg, &site, &ws.root, &sanitized, repo, event);
            if event == "up" && !routed_ok {
                result.demote(repo);
            }
        }
    }

    Ok(result)
}

/// Run a repo's opt-in `prepare` hook locally, ahead of `up` — runs on the client even for a remote workspace so it can stage client-side data (main-checkout db dump, local certs) into the worktree for the up-flush to ship. Returns `false` (and files the repo under `failed`) when the hook fails.
fn run_prepare(
    cfg: &config::WsConfig,
    root: &std::path::Path,
    wt: &std::path::Path,
    sanitized: &str,
    repo: &str,
    result: &mut SweepResult,
) -> bool {
    let script = match hooks::resolve(cfg, root, repo, "prepare", cfg.is_container_repo(repo, wt)) {
        Ok(Some(s)) => s,
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
    match hooks::run_command(
        root, wt, repo, sanitized, "prepare", &script.cmd, &extra_env, script.interactive,
    ) {
        Ok(()) => {
            step.ok();
            true
        }
        Err(e) => {
            step.fail(&format!("{e}"));
            result.file_failure(repo, &e);
            false
        }
    }
}

/// Handle one container repo on the server for a remote workspace: run its declarative script over SSH, then sync Caddy routes on the server. Mirrors the local container path but the script and routing run where the stack lives.
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

    // On `up`, `prepare` derives + writes the override by reading the worktree on the server — push edits first so a just-edited compose doesn't derive stale. Best-effort: the session may not exist yet.
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
    // Forward raw-TCP ports (db, redis, …) to localhost so a client-side DB GUI reaches the server's containers unchanged.
    if event == "down" || up_ok {
        sync_forwards(cfg, root, branch, sanitized, repo, event);
    }
}

/// Add (`up`) or remove (`down`) Caddy routes for a repo's HTTP services. Returns whether the sync succeeded — always `true` for `down` and for a repo with no HTTP routes; on `up`, `false` if any route failed to install (an HTTP container has no host-port fallback).
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
            Ok(()) => hosts.push(if r.wildcard { format!("*.{}", r.hostname) } else { r.hostname.clone() }),
            Err(e) => err = Some(format!("{e}")),
        }
    }
    let step = crate::ui::step(repo, "routed");
    match err {
        None => {
            // Caddy doesn't bind 443, so a bare hostname refuses the connection.
            let suffix = crate::caddy::port_suffix(crate::caddy::https_port());
            let with_port: Vec<String> = hosts.iter().map(|h| format!("{h}{suffix}")).collect();
            step.ok_with(&with_port.join(", "));
            true
        }
        Some(e) => {
            step.fail(&e);
            false
        }
    }
}

/// Forward a remote repo's allocated ports back to this machine (`up`) or tear them down (`down`), so each server-bound port is reachable at the same `localhost:<port>` used on the server. No-op for local; pure-HTTP services are reached via Caddy instead. Best-effort — a forward failure is noted, never fatal.
fn sync_forwards(
    cfg: &config::WsConfig,
    root: &std::path::Path,
    branch: &str,
    sanitized: &str,
    repo: &str,
    event: &str,
) {
    let Some(r) = cfg.remote.as_ref() else { return };
    // Successes batch into one aligned block — a stack with a dozen services buried the sweep otherwise; failures keep their own line so their error is readable.
    let mut forwarded: Vec<(String, String)> = Vec::new();
    for (svc, port) in crate::ports::load(root, sanitized, repo) {
        let session = crate::mutagen::forward_session_name(branch, repo, port);
        if event == "down" {
            let _ = crate::mutagen::forward_terminate(&session);
        } else {
            match crate::mutagen::forward_create(&session, &r.connection, port) {
                Ok(()) => forwarded.push((svc, format!("localhost:{port}"))),
                Err(e) => crate::ui::note(&format!("{svc}: forward localhost:{port} failed: {e}")),
            }
        }
    }
    crate::ui::table(repo, "port forwards", &forwarded);
}

/// Handle one host repo in a sweep: `up` starts its `up` script as a detached process; `down` stops it.
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
    let name = crate::supervise::proc_name(branch, repo);

    if event == "down" {
        let step = crate::ui::step(repo, "stop");
        result.record(repo, step, crate::supervise::down(site, &name));
        if cfg.routing_active() {
            // Best-effort (unlike a container): a host service keeps its real `DRUP_PORT`, so a route sync failure doesn't strand it.
            let _ = sync_routes(cfg, site, root, sanitized, repo, "down");
        }
        sync_forwards(cfg, root, branch, sanitized, repo, "down");
        return;
    }

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

    // Working dir at the execution site: server-side synced dir (absolute) for remote (flush first), else the local worktree. Both must be the site's paths — `DRUP_WORKSPACE_ROOT` is what a `.drup-hooks/…` script path anchors against, so a local root while running on the server points at a filesystem that isn't there.
    let (worktree, site_root) = match cfg.remote.as_ref() {
        Some(r) => {
            let _ = crate::mutagen::flush(&crate::mutagen::session_name(branch, repo));
            let base = site.abs_base(&r.worktree_base(root));
            // Ship `.drup-hooks/` so a host `up` referencing `$DRUP_WORKSPACE_ROOT/.drup-hooks/…` resolves on the server.
            if event == "up" {
                if let Err(e) = crate::remote::push_drup_hooks(site, root, &base) {
                    crate::ui::step(repo, &format!("{event} host")).fail(&format!("{e}"));
                    result.failed.push(repo.to_string());
                    return;
                }
            }
            (crate::remote::worktree_path(&base, sanitized, repo), base)
        }
        None => (wt.to_string_lossy().into_owned(), root.to_string_lossy().into_owned()),
    };

    // The detached path bypasses the `Command`-based hook runner, so apply the base `DRUP_*` contract here — otherwise `$DRUP_BRANCH_SANITIZED` & friends come out empty. Third time this path has been caught missing what `remote::run_event` already does; they are overdue for a shared owner.
    let mut proc_env =
        crate::hooks::base_env(std::path::Path::new(&site_root), &worktree, repo, sanitized, event);
    proc_env.extend(extra_env.iter().cloned());
    match crate::supervise::up(site, &name, &worktree, &cmd, &proc_env) {
        Ok(started) => {
            let label = if started { "start" } else { "already running (drup down to apply changes)" };
            // A host service has no hostname to show — the bound port is the only user-visible handle. Sorted for stable output.
            let mut ports: Vec<String> = extra_env
                .iter()
                .filter_map(|(k, v)| k.strip_prefix("DRUP_PORT_").map(|svc| format!("{svc}:{v}")))
                .collect();
            ports.sort();
            let step = crate::ui::step(repo, label);

            // Post-condition: `supervise::up` proves a process launched and survived half a second, not that it's serving the allocated port. Missed cases (server that bound a different port after finding this one busy, orphan still holding it) surfaced later as opaque failures.
            let allocated: Vec<u16> = extra_env
                .iter()
                .filter(|(k, _)| k.starts_with("DRUP_PORT_"))
                .filter_map(|(_, v)| v.parse().ok())
                .collect();
            let health = crate::supervise::health_settled(site, &name, &allocated, BIND_GRACE);
            let crashed = health == Some(crate::supervise::Health::Crashed);
            let squatted = matches!(&health, Some(crate::supervise::Health::Squatted(_)));

            match &health {
                // Died after launch = failed `up`, not a green line with caveats.
                Some(crate::supervise::Health::Crashed) => {
                    step.fail(&format!("exited after launch — see ~/.drup-run/{name}.log"));
                }
                // Alive but nothing on the port yet is genuinely ambiguous — `cargo watch` compiles before it listens — so qualify rather than fail.
                Some(crate::supervise::Health::Silent(_)) => {
                    step.ok_with(&format!("{} — not listening yet", ports.join(", ")));
                    crate::ui::note(&format!(
                        "{repo}: nothing is listening on the port drup allocated yet. If it's still \
                         starting this resolves itself — `drup list` will say. Otherwise check \
                         `~/.drup-run/{name}.log`."
                    ));
                }
                // A stranger on the port is unambiguous: whatever just started can't have it.
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

            if crashed {
                result.failed.push(repo.to_string());
                return;
            }
            result.ran.push(repo.to_string());
            if cfg.routing_active() {
                // Best-effort (see the `down` arm): a host service keeps its bound `DRUP_PORT`.
                let _ = sync_routes(cfg, site, root, sanitized, repo, "up");
            }
            // A squatted port belongs to some other process; forwarding `localhost:<port>` there would silently tunnel client traffic to the squatter.
            if !squatted {
                sync_forwards(cfg, root, branch, sanitized, repo, "up");
            }
        }
        Err(e) => {
            crate::ui::step(repo, "start").fail(&format!("{e}"));
            result.failed.push(repo.to_string());
        }
    }
}
