//! The body of both `ws up` and `ws down`; they differ only in event name,
//! header and summary. Traced end to end in ARCHITECTURE.md.
//!
//! Per-repo failures are isolated throughout, so one repo's broken hook doesn't
//! strand the others in the sweep.

use anyhow::Result;

use crate::config;
use crate::inject;
use crate::remote::Site;
use crate::workspace;

/// Long enough for an interpreted dev server (vite, `artisan serve`) to be
/// listening; a compiled one takes longer than any bearable wait, so raising
/// this wouldn't help it. Missing the window costs a qualified line, not a
/// failure.
const BIND_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// Outcome of a sweep, for the caller to render its own summary from.
pub struct SweepResult {
    pub ran: Vec<String>,
    /// Repos with a worktree but no hook for this event.
    pub skipped: Vec<String>,
    /// Non-empty ⇒ the op exits 1.
    pub failed: Vec<String>,
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
                self.failed.push(repo.to_string());
            }
        }
    }

    /// `None` means the repo has already been filed under `failed` and the
    /// caller should skip it — an injection failure is isolated per-repo, like a
    /// failing hook.
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

    /// A hook can succeed and the repo still not be up: the override drops
    /// the host publish, so a container whose Caddy route failed to install is
    /// unreachable.
    fn demote(&mut self, repo: &str) {
        self.ran.retain(|r| r != repo);
        if !self.failed.iter().any(|r| r == repo) {
            self.failed.push(repo.to_string());
        }
    }
}

/// Run `event`'s hook across every worktree of `branch`. A missing hook is a dim
/// skip; a failing hook is reported but does not stop the remaining repos.
pub fn sweep(branch: &str, event: &str, op: &str) -> Result<SweepResult> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;
    let sanitized = workspace::sanitize_branch(branch);
    // Resolving fetches the server `$HOME` up front, so an unreachable server
    // fails here with a clear message rather than mid-bring-up.
    let site = Site::resolve(&cfg)?;

    // Config order, for deterministic output.
    let targets: Vec<(String, std::path::PathBuf)> = cfg
        .repos
        .iter()
        .map(|r| r.name.as_str())
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

    // `down` is teardown-side: it reads the existing allocation, never makes one.
    let phase = if event == "up" { inject::Phase::Activate } else { inject::Phase::Inspect };

    // Deliberately not fatal: a sweep whose repos are all raw-TCP needs neither
    // Caddy nor `ws-net`, so a broken Caddy shouldn't block it. Where routing is
    // load-bearing the failure lands anyway — an HTTP container without `ws-net`
    // fails its own `compose up`, and one whose route won't install is demoted
    // below.
    if event == "up" {
        if let Some(domain) = cfg.domain.as_deref().filter(|_| cfg.routing_active()) {
            // `ensure_routing`'s `Dns` *is* the verdict, reused below rather than
            // resolving a second time.
            let configured = cfg.remote.as_ref().and_then(|r| r.publish_address.as_deref());
            let publish = crate::caddy::publish_address(&site, configured);
            // The all-interfaces default is right (offload is pointless if the
            // client can't reach it), but on a box with a public interface it
            // also serves every worktree to the internet, and nothing else here
            // would tell you that.
            if site.is_remote() && configured.is_none() {
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
                    // Caddy never came up, so nothing was resolved; assume
                    // unwired rather than invent a verdict.
                    crate::ui::note(&format!("routing infra not ready: {e:#}"));
                    crate::caddy::Dns::Unresolved
                }
            };
            // Remote nudges on the DNS verdict alone: it's already in hand for
            // free, it's the bit that usually isn't wired, and the full probe
            // would need the server address resolved and dial the server for CA
            // trust. Skipped entirely under a BYO Caddy.
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

        // A failed prepare skips the repo's up — no point bringing up a stack
        // whose seed never staged.
        if event == "up" && !run_prepare(&cfg, &ws.root, wt, branch, &sanitized, repo, &mut result) {
            continue;
        }

        // A host repo has no hook: its lifecycle is a detached ws-owned process
        // that has to survive `ws` exiting.
        if cfg.is_host_repo(repo) {
            sweep_host(&cfg, &site, &ws.root, wt, branch, &sanitized, repo, event, phase, &mut result);
            continue;
        }

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

        // `inject::prepare` derives services by reading the worktree *on the
        // server*, so a just-edited compose file would derive stale without this.
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
        if event == "down" || up_ok {
            sync_forwards(&cfg, &ws.root, branch, &sanitized, repo, event);
        }
    }

    Ok(result)
}

/// The one lifecycle hook that does *not* follow the stack: `prepare` is hard-wired
/// to `Site::Local` so it can stage data that only exists on the client (the main
/// checkout's live db, local certs) into the worktree, which the up dispatch's
/// flush then ships to the server. `WS_REMOTE` still reports the workspace's
/// remoteness, so one script can branch dump-for-sync vs same-host copy.
///
/// Returns `false` when the hook failed and the caller should skip the up; a
/// missing `prepare` is `true` and prints nothing.
fn run_prepare(
    cfg: &config::WsConfig,
    root: &std::path::Path,
    wt: &std::path::Path,
    branch: &str,
    sanitized: &str,
    repo: &str,
    result: &mut SweepResult,
) -> bool {
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

/// The hook paths only: a container's HTTP services are reachable *only* through
/// Caddy (the override drops their host publish), so a route that won't
/// install means an unreachable container and the repo is demoted. A host repo
/// keeps its bound `WS_PORT`, so `sweep_host` routes best-effort instead.
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

/// Returns whether the sync succeeded. Always `true` for `down` — route removal
/// is deliberately best-effort, since teardown must not be blockable.
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
            step.ok_with(&hosts.join(", "));
            true
        }
        Some(e) => {
            step.fail(&e);
            false
        }
    }
}

/// Makes each server-bound port reachable at the same `localhost:<port>` it uses
/// on the server, so a DB GUI on this machine works unchanged. A no-op locally.
///
/// Best-effort: a failure is noted, never fatal — the service is up on the server
/// regardless and re-running `up` retries.
///
/// Teardown re-derives the forwards from the `.ws-meta` port record, so it has to
/// run before `ports::forget` wipes it; a record deleted out of band strands its
/// forwards, the same window `ports::forget` itself has.
fn sync_forwards(
    cfg: &config::WsConfig,
    root: &std::path::Path,
    branch: &str,
    sanitized: &str,
    repo: &str,
    event: &str,
) {
    let Some(r) = cfg.remote.as_ref() else { return };
    if event == "down" {
        crate::mutagen::terminate_forwards(root, branch, sanitized, repo);
        return;
    }
    // Successes are batched into one aligned table because a dozen services
    // produced a dozen near-identical notes that buried the rest of the sweep.
    // Failures keep their own line: rare, and each carries an error to read.
    let mut forwarded: Vec<(String, String)> = Vec::new();
    for (svc, port) in crate::ports::load(root, sanitized, repo) {
        let session = crate::mutagen::forward_session_name(branch, repo, port);
        match crate::mutagen::forward_create(&session, &r.connection, port) {
            Ok(()) => forwarded.push((svc, format!("localhost:{port}"))),
            Err(e) => crate::ui::note(&format!("{svc}: forward localhost:{port} failed: {e}")),
        }
    }
    crate::ui::table(repo, "port forwards", &forwarded);
}

/// Handle one host repo: `up` starts its declared `up` script as a detached,
/// PID-file-tracked process with the injected env; `down` stops it.
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
            // Best-effort, unlike a container: a host service binds a real
            // `WS_PORT`, so a failed route only costs it the hostname.
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

    let Some(extra_env) = result.inject_env(cfg, site, root, wt, sanitized, repo, phase) else {
        return;
    };

    // Both must be paths at the *execution* site: `WS_WORKSPACE_ROOT` is what a
    // `.ws-hooks/…` script path anchors against, so a local root handed to a
    // process running on the server points at a filesystem that isn't there.
    let site_root = site.root_at(cfg.remote.as_ref(), root);
    let worktree = site.worktree_at(cfg.remote.as_ref(), root, wt, sanitized, repo);

    // The same staging dispatched lifecycle events get, borrowed rather than
    // respelled: flush the rendered templates and ship `.ws-hooks/`, so a host
    // `up` referencing `$WS_WORKSPACE_ROOT/.ws-hooks/…` resolves on the server.
    let ctx = crate::lifecycle::Ctx { cfg, site, root, worktree: wt, branch, sanitized, repo };
    if let Err(e) = ctx.push_to_server() {
        crate::ui::step(repo, &format!("{event} host")).fail(&format!("{e}"));
        result.failed.push(repo.to_string());
        return;
    }

    // A detached process doesn't go through the `Command`-based hook runner, so
    // the base `WS_*` contract is assembled here too — without it the `up` script
    // sees only `WS_PORT_*`/`WS_HOST_*` and `$WS_BRANCH_SANITIZED` comes out empty
    // (`https://.api.<domain>`). This is the one piece still written twice:
    // anything added to `hooks::base_env` has to reach both paths.
    let mut proc_env =
        crate::hooks::base_env(std::path::Path::new(&site_root), &worktree, repo, sanitized, event);
    proc_env.extend(extra_env.iter().cloned());
    match crate::supervise::up(site, &name, &worktree, &cmd, &proc_env) {
        Ok(started) => {
            let label = if started { "start" } else { "already running (ws down to apply changes)" };
            let (ports, allocated) = host_ports(&extra_env);
            let step = crate::ui::step(repo, label);

            // `supervise::up` proves a process launched and survived half a
            // second, not that it is serving the port ws allocated — a server
            // that quietly moved to another port after finding this one busy
            // still prints `start web:20001` and fails much later elsewhere.
            let health = crate::supervise::health_settled(site, &name, &allocated, BIND_GRACE);
            if report_host_health(step, repo, &name, &ports, health) {
                result.failed.push(repo.to_string());
                return;
            }
            result.ran.push(repo.to_string());
            if cfg.routing_active() {
                // Best-effort — see the `down` arm.
                let _ = sync_routes(cfg, site, root, sanitized, repo, "up");
            }
            sync_forwards(cfg, root, branch, sanitized, repo, "up");
        }
        Err(e) => {
            crate::ui::step(repo, "start").fail(&format!("{e}"));
            result.failed.push(repo.to_string());
        }
    }
}

/// Split a host repo's injected `WS_PORT_*` entries into the ports to display
/// (sorted, so the line is stable across runs) and the ports to probe. The two
/// sets deliberately differ: an entry whose value isn't a number still shows on
/// the line, but there's nothing there to probe.
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

/// Returns whether the process crashed after launch.
fn report_host_health(
    step: crate::ui::Step,
    repo: &str,
    name: &str,
    ports: &[String],
    health: Option<crate::supervise::Health>,
) -> bool {
    let crashed = health == Some(crate::supervise::Health::Crashed);

    match &health {
        // A failed `up`, not a green one with a caveat: there is nothing left
        // running to talk to.
        Some(crate::supervise::Health::Crashed) => {
            step.fail(&format!("exited after launch — see {}", crate::supervise::log_hint(name)));
        }
        // Genuinely ambiguous — a `cargo watch` compiles for a minute before it
        // listens — so qualify the claim rather than fail the repo.
        Some(crate::supervise::Health::Silent(_)) => {
            step.ok_with(&format!("{} — not listening yet", ports.join(", ")));
            crate::ui::note(&format!(
                "{repo}: nothing is listening on the port ws allocated yet. If it's still \
                 starting this resolves itself — `ws list` will say. Otherwise check \
                 `{log}`.",
                log = crate::supervise::log_hint(name),
            ));
        }
        // Not ambiguous: whatever just started cannot have the port, so it is
        // either dead or listening somewhere else.
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
