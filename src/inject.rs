//! Config injection — how per-worktree values reach the app, parser-free.
//!
//! `ws` allocates a hostname per HTTP service (`WS_HOST_<svc>` =
//! `<branch>.<subdomain>.<domain>`) and a host port per raw-TCP service
//! (`WS_PORT_<svc>`), then delivers them through three format-blind channels:
//!
//! 1. **Env vars** (primary) — the `WS_HOST_*`/`WS_PORT_*` vars plus the
//!    declared `env` mappings are set in the script/exec environment.
//! 2. **CLI-arg passthrough** (secondary) — needs no code here: an `up` script
//!    like `run -- --port=$WS_PORT_http` expands the var in its own `sh -c`.
//! 3. **Templates** (escape hatch) — a committed `src` is rendered to a
//!    gitignored `dest` with `${WS_*}` tokens substituted.
//!
//! A single entry point, [`prepare`], owns the whole protocol: a caller states
//! the lifecycle [`Phase`] and gets back the env pairs; whether ports are
//! allocated and templates rendered follows from the phase, so no caller has to
//! remember to pair a render call with an env call.
//!
//! Substitution is deliberately **brace-only and WS_-scoped**: only a
//! `${WS_...}` token naming a variable we own is replaced. Everything else —
//! nginx's `$host`, a shell `$FOO`, an unknown `${WS_typo}` — is copied
//! verbatim. (This is why we hand-roll rather than use the `subst` crate: `subst`
//! substitutes *every* `$VAR`, which would mangle the very config files —
//! nginx.conf, redis.conf — that channel 3 exists to render.)

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

use crate::compose;
use crate::config::{RepoConfig, RepoKind, ServiceConfig, WsConfig};
use crate::git;
use crate::ports;
use crate::remote::{self, Site};

/// The lifecycle phase an event belongs to. Decides port allocation and
/// template rendering so callers state intent, not mechanics.
#[derive(Clone, Copy)]
pub enum Phase {
    /// Bringing the stack up (`up` / `post-create`): allocate host ports and
    /// render templates.
    Activate,
    /// Running a command where the stack lives (`ws exec` locally): allocate
    /// ports so `WS_PORT_*` is present, but don't render templates — exec is a
    /// command runner, not a config step.
    Exec,
    /// Reading or tearing down (`down` / `post-remove` / `status` / remote
    /// exec): use whatever's already allocated; never allocate or render.
    Inspect,
}

impl Phase {
    fn allocates(self) -> bool {
        matches!(self, Phase::Activate | Phase::Exec)
    }
    fn renders(self) -> bool {
        matches!(self, Phase::Activate)
    }
    /// Whether to auto-derive services from `compose config`. Only when
    /// activating — deriving shells out to the runtime, too heavy for the
    /// read-only `ws list`/exec paths, which use the declared `services` and the
    /// already-recorded ports.
    fn derives(self) -> bool {
        matches!(self, Phase::Activate)
    }
    /// Whether to (re)write the container compose override. Only when
    /// activating — the same activating concept as `renders`/`derives`, but
    /// named on its own so the override's write-vs-reuse decision reads
    /// independently (read/teardown phases reuse the file the last `up` wrote).
    fn writes_override(self) -> bool {
        matches!(self, Phase::Activate)
    }
}

/// Compute the injection env (`WS_HOST_*`/`WS_PORT_*` + declared `env` mappings)
/// for a repo's worktree, allocating host ports and rendering templates as the
/// `phase` dictates. A bare-name repo, or one with no services/env, yields an
/// empty vec. This is the one function every op calls.
///
/// Location-transparent via `site`: derivation, port-freeness probes, and the
/// generated compose override all happen where the stack runs (locally, or on
/// the server for a remote workspace) — `worktree` is always the local (synced)
/// path, used for the file checks and template rendering that `ws` does host-side.
pub fn prepare(
    cfg: &WsConfig,
    site: &Site,
    root: &Path,
    worktree: &Path,
    branch_sanitized: &str,
    repo: &str,
    phase: Phase,
) -> Result<Vec<(String, String)>> {
    // Workspace-context vars every hook receives, independent of repo config or
    // routing: the container runtime (so a shared hook can call `$WS_RUNTIME
    // compose` instead of hardcoding podman/docker) and whether the workspace
    // offloads (so a hook can branch local-vs-remote behaviour — e.g. same-host db
    // copy locally vs importing a synced dump on the server). Emitted even for a
    // bare-name repo, which returns early below with no routing env.
    let context = workspace_context(cfg);
    let Some(rc) = cfg.repo_config(repo) else { return Ok(context) };

    // The worktree path at the execution site: the local path, or the synced dir
    // on the server (absolute — see `site_worktree_path`). `compose config`/the
    // override run there; file checks and template renders below stay on the local
    // (synced) copy.
    let site_worktree = site_worktree_path(cfg, site, root, worktree, branch_sanitized, repo);

    // On an activating event the live compose derivation is authoritative
    // (declared `services` overlaid on top); on read-only phases prefer the
    // effective set recorded at the last `up` — it covers services that were
    // auto-derived from compose and never declared — falling back to the
    // declared block for a worktree that predates the record.
    let (services, all_services): (BTreeMap<String, ServiceConfig>, std::collections::BTreeSet<String>) =
        if phase.derives() {
            effective_services(cfg, site, &site_worktree, worktree, repo, rc)?
        } else {
            let recorded = ports::services(root, branch_sanitized, repo);
            let s = if recorded.is_empty() { rc.services.clone() } else { recorded };
            let names = s.keys().cloned().collect();
            (s, names)
        };

    // Allocate a host port per raw-TCP service (host repos: every service). The
    // record + lock stay host-side (the host drives every `ws` call); only the
    // freeness probe follows `site`, so a remote service is de-conflicted on the
    // server that publishes it. `WS_HOST_*` needs no port — the routed hostname is
    // location-transparent.
    let host = rc.is_host();
    let port_map = if phase.allocates() {
        // A container only needs a host port for its raw-TCP services — HTTP
        // ones are dialed on the container network. An explicit host repo
        // allocates one for EVERY service: N host processes on a single machine
        // can't share a port the way N isolated containers can, so even an HTTP
        // host service needs its own port to bind (Caddy then routes the host to
        // `host.containers.internal:<port>`).
        let needs: Vec<String> = services
            .iter()
            .filter(|(_, s)| host || !s.is_http())
            .map(|(n, _)| n.clone())
            .collect();
        ports::resolve(site, root, branch_sanitized, repo, &needs)?
    } else {
        ports::load(root, branch_sanitized, repo)
    };

    // Record the effective service set at `up` so the read-only phases above can
    // reconstruct hosts/ports for auto-derived services.
    if phase.derives() {
        ports::record_services(root, branch_sanitized, repo, &services)?;
    }

    let vars = ws_vars(&services, branch_sanitized, cfg.domain.as_deref(), &port_map, host);

    // Substitution map = this repo's own `WS_*` plus every *peer* repo's, re-keyed
    // under a `<repo>.` prefix (`${WS_PORT_app.web}`, `${WS_HOST_app.web}`), so a
    // repo's `env`/`templates` can reference a sibling's routed host or allocated
    // port. Peer entries are for substitution ONLY — their keys carry a `.` (not a
    // valid env-var name) and are never injected. Peers are read-only: their ports
    // come from the allocation recorded at `ws new`, so an unbuilt peer just leaves
    // the token literal (same as any unknown `${WS_*}`).
    let mut subst = vars.clone();
    // The workspace context (`WS_RUNTIME`, `WS_REMOTE`, `WS_CONNECTION`) is
    // substitutable too. Without it the only way to hand a project ws's answer for
    // something is to teach the project about ws — a tool that shells out to a
    // container runtime has to learn `WS_RUNTIME`, which puts ws's vocabulary in a
    // codebase that should not have to know it exists. Declaring
    // `"DOCKER_COMMAND": "${WS_RUNTIME}"` keeps that mapping in `.ws.json`, which
    // is where the coupling belongs. No collision risk: these keys are fixed and
    // distinct from the `WS_HOST_*`/`WS_PORT_*` above.
    subst.extend(context.iter().cloned());
    for peer in cfg.repos.iter().map(|r| r.name()).filter(|&n| n != repo) {
        subst.extend(peer_vars(cfg, site, root, branch_sanitized, peer, phase)?);
    }

    if phase.renders() {
        render_all(worktree, rc, &subst)?;
    }

    // Declared `env` mappings with their `${WS_*}` tokens substituted. Fed both
    // into the script env and (for a container) the override's `environment:`.
    let declared = declared_env(rc, &subst);
    let mut env: Vec<(String, String)> =
        vars.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    env.extend(declared.iter().cloned());

    // Container repos get the generated compose override + project namespacing
    // via `COMPOSE_*` env, so N parallel worktrees de-conflict their raw-TCP
    // host ports. Host repos (not compose) are excluded. For a remote workspace
    // the override is written on the server and `COMPOSE_FILE` uses server paths.
    if !host && rc.is_container(|| compose::has_compose_file(worktree)) {
        let project = compose::project_name(branch_sanitized, repo);
        // Join `ws-net` (so Caddy can dial the HTTP services) only when routing
        // is active. Without it, referencing the external `ws-net` would just
        // make `compose up` fail for nothing. The op layer (`ws up`/`new`)
        // ensures the network exists before this override is used.
        let join_ws_net = cfg.routing_active();
        let base = base_files_at(worktree, &site_worktree);
        let override_path = override_path_at(cfg, site, root, branch_sanitized, repo);
        env.extend(compose_env(
            site, &base, &override_path, &project, &services, &all_services, &port_map, &declared,
            phase, join_ws_net,
        )?);
    }
    env.extend(context);
    Ok(env)
}

/// The workspace-context env pairs (`WS_RUNTIME`, `WS_REMOTE`, `WS_CONNECTION`)
/// every hook sees. `WS_REMOTE` reflects whether the *workspace* offloads
/// (`cfg.remote`), not the site a given call runs against, so a hook reads one
/// consistent answer wherever it executes. `1`/`0` for shell-friendly
/// `[ "$WS_REMOTE" = 1 ]` tests.
///
/// `WS_CONNECTION` is the SSH destination, empty for a local workspace. Without
/// it a **client-side** hook knows it is offloading but not to where, so it
/// cannot reach the server without the user hardcoding a hostname into a script
/// that `.ws.json` already names. That closes the loop on client-side state the
/// server needs but no repo contains — a dev CA, a registry token, an auth.json:
/// `prepare` runs on the client before every `up`, and can now push it.
///
/// Derived from config rather than declared, and deliberately not named
/// `WS_REMOTE_HOST` — that is an *input* override `ws` itself reads, and a hook
/// inheriting it would silently re-target a nested `ws` call.
fn workspace_context(cfg: &WsConfig) -> Vec<(String, String)> {
    vec![
        ("WS_RUNTIME".to_string(), cfg.runtime().to_string()),
        ("WS_REMOTE".to_string(), if cfg.remote.is_some() { "1" } else { "0" }.to_string()),
        (
            "WS_CONNECTION".to_string(),
            cfg.remote.as_ref().map(|r| r.connection.clone()).unwrap_or_default(),
        ),
    ]
}

/// The worktree path at the execution site: the local path for a local
/// workspace, or the server-side synced dir (`<workspace_path>/<task>/<repo>`)
/// for a remote one.
fn site_worktree_path(
    cfg: &WsConfig,
    site: &Site,
    root: &Path,
    worktree: &Path,
    branch_sanitized: &str,
    repo: &str,
) -> String {
    match &cfg.remote {
        None => worktree.to_string_lossy().into_owned(),
        // Absolute (home-resolved): the server dir feeds `COMPOSE_FILE`, which must
        // be absolute or the `cd`'d up script would double-resolve it.
        Some(r) => {
            remote::worktree_path(&site.abs_base(&r.worktree_base(root)), branch_sanitized, repo)
        }
    }
}

/// The base compose file(s) as paths *at the site*: `compose::base_files` finds
/// which files exist in the local (synced) worktree, then each is re-homed under
/// `site_worktree` (a no-op for a local site, where the two dirs coincide).
fn base_files_at(worktree: &Path, site_worktree: &str) -> Vec<String> {
    let base = site_worktree.trim_end_matches('/');
    compose::base_files(worktree)
        .iter()
        .filter_map(|p| p.file_name().map(|n| format!("{base}/{}", n.to_string_lossy())))
        .collect()
}

/// The generated compose override's path at the execution site. Local: under the
/// host `.ws-meta`. Remote: under a `.ws-meta` sibling of the server's synced
/// worktrees (`<workspace_path>/.ws-meta/...`), so it lives beside the stack it
/// overrides and is torn down with it. `pub(crate)` so teardown can remove the
/// server-side file it wrote.
pub(crate) fn override_path_at(
    cfg: &WsConfig,
    site: &Site,
    root: &Path,
    branch_sanitized: &str,
    repo: &str,
) -> String {
    match &cfg.remote {
        None => ports::override_file(root, branch_sanitized, repo).to_string_lossy().into_owned(),
        // Absolute (home-resolved), same reason as `site_worktree_path`.
        Some(r) => format!(
            "{}/.ws-meta/{branch_sanitized}-{repo}.override.yml",
            site.abs_base(&r.worktree_base(root)).trim_end_matches('/')
        ),
    }
}

/// The `COMPOSE_*` env for a container repo. `COMPOSE_PROJECT_NAME` is set on
/// every phase (isolation — compose's default project name is the worktree's
/// dir basename `api`, identical across branches, so distinct stacks would
/// collide on containers/networks/volumes). `COMPOSE_FILE` (base files plus the
/// generated override) is set **only on an activating phase**, which is also the
/// only phase that (re)writes the override: read/teardown/exec phases don't
/// re-create containers — `down`/`ps`/`exec` target the project by name, and the
/// override carries only ports/env — so they leave `COMPOSE_FILE` unset rather
/// than risk referencing an override an edited compose file left stale.
///
/// Empty when no compose file is present (an explicit `kind: container` with
/// nothing to find — the derive path already surfaces that fatally on activation).
#[allow(clippy::too_many_arguments)]
fn compose_env(
    site: &Site,
    base: &[String],
    override_path: &str,
    project: &str,
    services: &BTreeMap<String, ServiceConfig>,
    all_services: &std::collections::BTreeSet<String>,
    port_map: &BTreeMap<String, u16>,
    declared: &[(String, String)],
    phase: Phase,
    join_ws_net: bool,
) -> Result<Vec<(String, String)>> {
    if base.is_empty() {
        return Ok(Vec::new());
    }

    let mut env = vec![("COMPOSE_PROJECT_NAME".to_string(), project.to_string())];
    if phase.writes_override()
        && compose::write_override(site, override_path, services, all_services, port_map, declared, join_ws_net)?
    {
        let mut files: Vec<String> = base.to_vec();
        files.push(override_path.to_string());
        env.push(("COMPOSE_FILE".to_string(), files.join(":")));
    }
    Ok(env)
}

/// The Caddy routes for a repo's HTTP services: one per HTTP service, matching
/// `<branch>.<subdomain>.<domain>` and proxying to the worktree's container
/// (`<project>-<service>-1:<port>`) or host process
/// (`host.containers.internal:<WS_PORT>`).
///
/// Read-only: it uses the effective service set recorded at the last `up` (so
/// `down`/`rm` resolve the same `@id`s `up` created) and never allocates or
/// derives. Empty when the repo has no config or no `domain` is set (no routing).
pub fn routes(
    cfg: &WsConfig,
    root: &Path,
    branch_sanitized: &str,
    repo: &str,
) -> Vec<crate::caddy::RouteSpec> {
    let (Some(rc), Some(domain)) = (cfg.repo_config(repo), cfg.domain.as_deref()) else {
        return Vec::new();
    };
    // Prefer the set recorded at `up` (covers auto-derived services); fall back
    // to the declared block for a worktree that predates the record.
    let recorded = ports::services(root, branch_sanitized, repo);
    let services = if recorded.is_empty() { rc.services.clone() } else { recorded };
    // Only the branch actually taken pays its lookup: a host repo loads the port
    // map (upstream is its allocated port); a container repo computes the project
    // name (upstream is the project-qualified container name).
    let host = rc.is_host();
    let port_map = if host { ports::load(root, branch_sanitized, repo) } else { BTreeMap::new() };
    let project = if host { String::new() } else { compose::project_name(branch_sanitized, repo) };

    route_specs(&services, &port_map, host, &project, branch_sanitized, repo, domain)
}

/// The pure core of [`routes`]: given a resolved service set, build one route
/// spec per HTTP service. A host service without an allocated port is skipped.
fn route_specs(
    services: &BTreeMap<String, ServiceConfig>,
    port_map: &BTreeMap<String, u16>,
    host: bool,
    project: &str,
    branch_sanitized: &str,
    repo: &str,
    domain: &str,
) -> Vec<crate::caddy::RouteSpec> {
    let mut out = Vec::new();
    for (name, svc) in services {
        let Some(sub) = svc.subdomain(name) else { continue }; // HTTP services only
        let hostname = ws_host(branch_sanitized, sub, domain);
        let upstream = if host {
            // A host process is reached at its allocated port on the host.
            match port_map.get(name) {
                Some(port) => format!("host.containers.internal:{port}"),
                None => continue,
            }
        } else {
            // A container is dialed by its project-qualified name on `ws-net`.
            format!("{project}-{}-1:{}", compose::compose_service(name, svc.port), svc.port)
        };
        out.push(crate::caddy::RouteSpec {
            id: crate::caddy::route_id(branch_sanitized, repo, name),
            hostname,
            upstream,
            wildcard: svc.wildcard,
        });
    }
    out
}

/// The effective services for a repo: for a container repo (explicit `kind` or a
/// compose file present), the live `compose config` derivation with the declared
/// `services` overlaid on top (declared wins per key); otherwise the declared
/// block alone.
///
/// A failed derivation is fatal for an **explicit** `kind: container` — the user
/// said it's a container, so a broken compose must surface, not silently bring
/// up an unrouted stack. When container-ness was merely *inferred* from a compose
/// file, we fall back to the declared block with a note (the inference is a guess,
/// and the local-first path shouldn't hard-fail just because the runtime is out).
fn effective_services(
    cfg: &WsConfig,
    site: &Site,
    site_worktree: &str,
    local_worktree: &Path,
    repo: &str,
    rc: &RepoConfig,
) -> Result<(BTreeMap<String, ServiceConfig>, std::collections::BTreeSet<String>)> {
    if rc.is_container(|| compose::has_compose_file(local_worktree)) {
        match compose::derive(site, cfg.runtime(), site_worktree) {
            Ok(d) => {
                let mut services = d.services;
                services.extend(rc.services.clone());
                return Ok((services, d.names));
            }
            Err(e) if rc.kind == Some(RepoKind::Container) => {
                return Err(e.context(format!("{repo}: `{}` is a container repo but compose config failed", repo)));
            }
            Err(e) => {
                crate::ui::note(&format!("{repo}: compose derive failed ({e}); using declared services"));
            }
        }
    }
    // No derive (host repo, or a failed/absent compose config): the declared set
    // is all we know, and its keys double as the service-name list.
    Ok((rc.services.clone(), rc.services.keys().cloned().collect()))
}

/// The `WS_HOST_<svc>` / `WS_PORT_<svc>` variable map, keyed off the *current*
/// service set and the current allocation policy — never the raw `.ws-meta` map,
/// which can retain a stale port for a service that was since removed or flipped
/// to HTTP. HTTP services yield a hostname (only when a top-level `domain` is
/// set). `WS_PORT` is emitted only when the service is one `ws` currently
/// allocates a port for (`host` repo → every service; container → raw-TCP only),
/// so a host HTTP service gets both while a container HTTP service never gets a
/// (possibly stale) port.
/// The per-worktree hostname for an HTTP service: `<branch>.<subdomain>.<domain>`.
/// Shared by the `WS_HOST_*` env var and the Caddy route match so the two can't
/// drift.
fn ws_host(branch_sanitized: &str, subdomain: &str, domain: &str) -> String {
    format!("{}.{}.{domain}", dns_label(branch_sanitized), dns_label(subdomain))
}

/// Collapse a branch/subdomain into a single DNS label. `sanitize_branch` leaves
/// dots (`v2.1`, `release/1.2` → `release-1.2`), which add extra hostname labels —
/// harmless for an exact match, but with a wildcard route `*.<branch>.<sub>.<domain>`
/// a dotted branch's base host (`a.b.<sub>.<domain>`) would be *suffix-captured* by
/// a sibling branch `b`'s wildcard, cross-routing worktrees. One label, no capture.
fn dns_label(s: &str) -> String {
    s.replace('.', "-")
}

fn ws_vars(
    services: &BTreeMap<String, ServiceConfig>,
    branch_sanitized: &str,
    domain: Option<&str>,
    port_map: &BTreeMap<String, u16>,
    host: bool,
) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();
    for (name, svc) in services {
        if let Some(sub) = svc.subdomain(name) {
            if let Some(d) = domain {
                vars.insert(format!("WS_HOST_{name}"), ws_host(branch_sanitized, sub, d));
            }
        }
        if host || !svc.is_http() {
            if let Some(port) = port_map.get(name) {
                vars.insert(format!("WS_PORT_{name}"), port.to_string());
            }
        }
    }
    vars
}

/// A peer repo's `WS_HOST_*`/`WS_PORT_*`, re-keyed under a `<peer>.` prefix
/// (`WS_PORT_<peer>.<svc>`) so another repo can reference it via substitution.
/// Read-only: the peer's ports come from its recorded allocation (written at
/// `ws new`) — never a fresh allocation here. Empty when the peer isn't configured
/// or has no allocation yet, which leaves the referencing token literal.
fn peer_vars(
    cfg: &WsConfig,
    site: &Site,
    root: &Path,
    branch_sanitized: &str,
    peer: &str,
    phase: Phase,
) -> Result<BTreeMap<String, String>> {
    let Some(rc) = cfg.repo_config(peer) else { return Ok(BTreeMap::new()) };
    // Prefer the service set recorded at the peer's last `up` (covers auto-derived
    // container services); fall back to its declared block for a fresh worktree.
    let recorded = ports::services(root, branch_sanitized, peer);
    let services = if recorded.is_empty() { rc.services.clone() } else { recorded };
    let peer_host = rc.is_host();

    // On an allocating phase (`up`/`new`/`exec`) **pre-allocate** the peer's
    // port-bearing services, so `${WS_PORT_<peer>.<svc>}` resolves even when the
    // peer worktree isn't built yet — the whole point of letting a repo reference
    // a sibling it's spun up separately from. The reservation lands in the peer's
    // own `.ws-meta`; when the peer is later brought up, `resolve` preserves it, so
    // both agree on the port regardless of order. Teardown of the branch frees a
    // reservation whose peer was never created. Read/teardown phases stay
    // read-only (`load`) — they must never allocate. `WS_HOST_*` needs no port and
    // resolved without this already; this is purely for `WS_PORT_*`.
    let port_map = if phase.allocates() {
        let needs: Vec<String> = services
            .iter()
            .filter(|(_, s)| peer_host || !s.is_http())
            .map(|(n, _)| n.clone())
            .collect();
        ports::resolve(site, root, branch_sanitized, peer, &needs)?
    } else {
        ports::load(root, branch_sanitized, peer)
    };
    Ok(ws_vars(&services, branch_sanitized, cfg.domain.as_deref(), &port_map, peer_host)
        .into_iter()
        .filter_map(|(k, v)| {
            let rekeyed = k
                .strip_prefix("WS_HOST_")
                .map(|svc| format!("WS_HOST_{peer}.{svc}"))
                .or_else(|| k.strip_prefix("WS_PORT_").map(|svc| format!("WS_PORT_{peer}.{svc}")))?;
            Some((rekeyed, v))
        })
        .collect())
}

/// The declared `env` entries with their `${WS_*}` tokens substituted, in
/// deterministic key order (`rc.env` is a `BTreeMap`). `ws` owns the `WS_*`
/// values in `vars`; these map them onto the app's own var names.
fn declared_env(rc: &RepoConfig, vars: &BTreeMap<String, String>) -> Vec<(String, String)> {
    rc.env.iter().map(|(k, t)| (k.clone(), substitute(t, vars))).collect()
}

/// Render every declared template into the worktree, then register all of their
/// dests in `info/exclude` in one pass (one `git` spawn, one file rewrite).
fn render_all(worktree: &Path, rc: &RepoConfig, vars: &BTreeMap<String, String>) -> Result<()> {
    if rc.templates.is_empty() {
        return Ok(());
    }
    let mut dests = Vec::with_capacity(rc.templates.len());
    for t in &rc.templates {
        render_one(worktree, &t.src, &t.dest, vars)?;
        dests.push(t.dest.clone());
    }
    git::add_worktree_excludes(worktree, &dests)
}

/// Render one template: read `src`, substitute, write `dest`. Refuses if `dest`
/// is tracked (rendering would dirty a committed file). Paths are
/// worktree-relative; keeping `dest` git-invisible is the caller's job.
fn render_one(
    worktree: &Path,
    src: &str,
    dest: &str,
    vars: &BTreeMap<String, String>,
) -> Result<()> {
    if git::is_tracked(worktree, dest) {
        bail!(
            "template dest `{dest}` is tracked by git — rendering it would dirty the repo; \
             gitignore it and `git rm --cached {dest}` first"
        );
    }
    let src_path = worktree.join(src);
    let raw = std::fs::read_to_string(&src_path)
        .with_context(|| format!("read template {}", src_path.display()))?;
    let rendered = substitute(&raw, vars);

    let dest_path = worktree.join(dest);
    if let Some(parent) = dest_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&dest_path, rendered)
        .with_context(|| format!("write {}", dest_path.display()))
}

/// Replace `${NAME}` tokens with values from `vars`, but only for names present
/// in the map (which only ever holds `WS_*` keys). Any other `$…` — a bare
/// `$FOO`, an unknown `${WS_typo}`, nginx's `$host` — is left byte-for-byte.
fn substitute(text: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find("${") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 2..];
        if let Some(close) = after.find('}') {
            if let Some(val) = vars.get(&after[..close]) {
                out.push_str(val);
                rest = &after[close + 1..];
                continue;
            }
        }
        // Not a known `${WS_*}` token — emit the literal `${` and keep scanning.
        out.push_str("${");
        rest = after;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HttpDecl, ServiceConfig};

    fn svc(http: Option<HttpDecl>, port: u16) -> ServiceConfig {
        ServiceConfig { http, port, shared: false, wildcard: false }
    }

    fn repo_with(services: &[(&str, ServiceConfig)]) -> RepoConfig {
        RepoConfig {
            name: "api".into(),
            kind: None,
            services: services.iter().map(|(n, s)| ((*n).into(), s.clone())).collect(),
            scripts: Default::default(),
            env: Default::default(),
            templates: Vec::new(),
            hooks: Default::default(),
            worktree_excludes: Vec::new(),
            copy: Vec::new(),
        }
    }

    /// The repo's declared services as the sorted map `ws_vars` now takes.
    fn services(rc: &RepoConfig) -> BTreeMap<String, ServiceConfig> {
        rc.services.clone()
    }

    #[test]
    fn workspace_context_reports_runtime_and_remoteness() {
        use crate::config::{RemoteConfig, WsConfig};

        // Default runtime, local workspace.
        let local = WsConfig::default();
        let ctx = workspace_context(&local);
        assert!(ctx.contains(&("WS_RUNTIME".to_string(), "podman".to_string())), "{ctx:?}");
        assert!(ctx.contains(&("WS_REMOTE".to_string(), "0".to_string())), "{ctx:?}");
        // Always present, so a hook can test it without an unset-variable guard.
        assert!(ctx.contains(&("WS_CONNECTION".to_string(), String::new())), "{ctx:?}");

        // Explicit runtime + a remote block flips both.
        let remote = WsConfig {
            runtime: Some("docker".to_string()),
            remote: Some(RemoteConfig::new("srv".to_string(), None)),
            ..WsConfig::default()
        };
        let ctx = workspace_context(&remote);
        assert!(ctx.contains(&("WS_RUNTIME".to_string(), "docker".to_string())), "{ctx:?}");
        assert!(ctx.contains(&("WS_REMOTE".to_string(), "1".to_string())), "{ctx:?}");
        // The destination, so a client-side `prepare` can actually reach the server.
        assert!(ctx.contains(&("WS_CONNECTION".to_string(), "srv".to_string())), "{ctx:?}");
    }

    #[test]
    fn substitute_resolves_a_cross_repo_token() {
        let mut vars = BTreeMap::new();
        vars.insert("WS_PORT_app.web".to_string(), "20008".to_string());
        assert_eq!(
            substitute("https://myapp.localhost:${WS_PORT_app.web}/x", &vars),
            "https://myapp.localhost:20008/x"
        );
        // An unbuilt/unknown peer leaves the token literal, like any unknown `${WS_*}`.
        assert_eq!(
            substitute("${WS_PORT_missing.web}", &BTreeMap::new()),
            "${WS_PORT_missing.web}"
        );
    }

    #[test]
    fn peer_vars_rekeys_a_recorded_peer_port() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cfg: WsConfig = serde_json::from_str(
            r#"{"repos":[{"name":"api","kind":"container"},
                         {"name":"app","kind":"host","services":{"web":{"port":8000}}}]}"#,
        )
        .unwrap();
        // Record the app's allocation the way `ws new` would.
        std::fs::create_dir_all(crate::ports::meta_dir(root)).unwrap();
        std::fs::write(
            crate::ports::meta_dir(root).join("feat-app.json"),
            r#"{"ports":{"web":20008}}"#,
        )
        .unwrap();

        // Read-only phase: use the recorded port, allocate nothing.
        let pv = peer_vars(&cfg, &Site::Local, root, "feat", "app", Phase::Inspect).unwrap();
        assert_eq!(pv.get("WS_PORT_app.web").map(String::as_str), Some("20008"));
        // The app's web service isn't routed, so it exposes no host var.
        assert!(!pv.contains_key("WS_HOST_app.web"));
    }

    #[test]
    fn peer_vars_read_phase_is_empty_for_an_unbuilt_or_unknown_peer() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cfg: WsConfig = serde_json::from_str(
            r#"{"repos":[{"name":"api","kind":"container"},
                         {"name":"app","kind":"host","services":{"web":{"port":8000}}}]}"#,
        )
        .unwrap();
        // Read-only phase, never built (no `.ws-meta`) ⇒ no recorded port, no alloc.
        assert!(peer_vars(&cfg, &Site::Local, root, "feat", "app", Phase::Inspect).unwrap().is_empty());
        // Not a repo at all ⇒ empty on any phase.
        assert!(peer_vars(&cfg, &Site::Local, root, "feat", "nope", Phase::Activate).unwrap().is_empty());
    }

    #[test]
    fn peer_vars_pre_allocates_an_unbuilt_peer_on_an_allocating_phase() {
        // The point of the feature: `ws up api` (Activate) must resolve
        // `${WS_PORT_app.web}` even though the app worktree doesn't exist — by
        // reserving app's port. The reservation persists so app reuses it later.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cfg: WsConfig = serde_json::from_str(
            r#"{"repos":[{"name":"api","kind":"container"},
                         {"name":"app","kind":"host","services":{"web":{"port":8000}}}]}"#,
        )
        .unwrap();

        let pv = peer_vars(&cfg, &Site::Local, root, "feat", "app", Phase::Activate).unwrap();
        let port = pv.get("WS_PORT_app.web").expect("peer port pre-allocated");
        assert!(port.parse::<u16>().is_ok(), "a real port, got {port}");
        // It landed in the peer's own `.ws-meta`, so app reuses it when built.
        assert_eq!(
            ports::load(root, "feat", "app").get("web").map(|p| p.to_string()).as_deref(),
            Some(port.as_str()),
        );
    }

    #[test]
    fn route_specs_container_upstreams_and_http_filter() {
        // web (HTTP 80) + admin (HTTP 8080) + mysql (raw-TCP) — only the HTTP
        // ones route, each dialing its project-qualified container name.
        let services = services(&repo_with(&[
            ("web", svc(Some(HttpDecl::Subdomain("web".into())), 80)),
            ("admin", svc(Some(HttpDecl::Enabled(true)), 8080)),
            ("mysql", svc(None, 3306)),
        ]));
        let specs = route_specs(
            &services, &BTreeMap::new(), false, "feat-x-api", "feat-x", "api", "myapp.test",
        );
        assert_eq!(specs.len(), 2, "only HTTP services route: {:?}", specs.iter().map(|r| &r.id).collect::<Vec<_>>());
        let web = specs.iter().find(|r| r.id == "ws-feat-x-api-web").unwrap();
        assert_eq!(web.hostname, "feat-x.web.myapp.test");
        assert_eq!(web.upstream, "feat-x-api-web-1:80");
        let admin = specs.iter().find(|r| r.id == "ws-feat-x-api-admin").unwrap();
        assert_eq!(admin.hostname, "feat-x.admin.myapp.test");
        assert_eq!(admin.upstream, "feat-x-api-admin-1:8080");
    }

    #[test]
    fn route_specs_host_upstreams_and_drop_when_no_port() {
        let services = services(&repo_with(&[
            ("web", svc(Some(HttpDecl::Subdomain("web".into())), 8080)),
            ("api", svc(Some(HttpDecl::Enabled(true)), 9000)),
        ]));
        // Host repo: upstream is the allocated port via host.containers.internal.
        // Only `web` has an allocation → `api` (no port) is dropped.
        let ports = [("web".to_string(), 20001u16)].into_iter().collect();
        let specs = route_specs(&services, &ports, true, "", "feat-x", "svc", "myapp.test");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].id, "ws-feat-x-svc-web");
        assert_eq!(specs[0].upstream, "host.containers.internal:20001");
        assert!(!specs[0].wildcard, "web is not a wildcard service");
    }

    #[test]
    fn a_wildcard_service_carries_the_flag_into_its_route_spec() {
        // A multi-tenant host service (e.g. the Vue app): `<tenant>.<host>` routes
        // to it for any tenant. The base hostname is the usual `<branch>.<sub>.<domain>`;
        // the wildcard flag is what tells Caddy to also match every tenant label.
        let mut app = svc(Some(HttpDecl::Subdomain("app".into())), 3000);
        app.wildcard = true;
        let services = services(&repo_with(&[("app", app)]));
        let ports = [("app".to_string(), 20005u16)].into_iter().collect();
        let specs = route_specs(&services, &ports, true, "", "feat-x", "app", "myapp.test");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].hostname, "feat-x.app.myapp.test");
        assert!(specs[0].wildcard, "wildcard service must set the route-spec flag");
    }

    #[test]
    fn wildcard_on_a_raw_tcp_service_yields_no_route() {
        // `wildcard` is HTTP-only: a raw-TCP service gets an allocated port, not a
        // hostname, so the flag is a silent no-op (no route at all) — never a
        // hostname-less or bogus route.
        let mut db = svc(None, 5432);
        db.wildcard = true;
        let services = services(&repo_with(&[("db", db)]));
        let ports = [("db".to_string(), 20009u16)].into_iter().collect();
        assert!(route_specs(&services, &ports, false, "proj", "feat-x", "api", "myapp.test").is_empty());
    }

    #[test]
    fn a_dotted_branch_collapses_to_one_hostname_label() {
        // A dotted branch (`v2.1`) must not add extra hostname labels: with a
        // wildcard route it would otherwise let a sibling branch's `*.` suffix-
        // capture this worktree's traffic.
        assert_eq!(ws_host("v2.1", "web", "myapp.test"), "v2-1.web.myapp.test");
        assert_eq!(ws_host("release-1.2", "a.b", "d.test"), "release-1-2.a-b.d.test");
    }

    #[test]
    fn substitute_only_touches_known_ws_braces() {
        let mut vars = BTreeMap::new();
        vars.insert("WS_HOST_app".to_string(), "feat.api.myapp.test".to_string());
        vars.insert("WS_PORT_mysql".to_string(), "54321".to_string());

        assert_eq!(
            substitute("https://${WS_HOST_app}/x", &vars),
            "https://feat.api.myapp.test/x"
        );
        // Unknown brace var, bare $VAR, and nginx-style $host are untouched.
        assert_eq!(substitute("${WS_typo}", &vars), "${WS_typo}");
        assert_eq!(substitute("$WS_HOST_app", &vars), "$WS_HOST_app");
        assert_eq!(
            substitute("listen $host; port ${WS_PORT_mysql};", &vars),
            "listen $host; port 54321;"
        );
        // A dangling `${` with no close is copied verbatim.
        assert_eq!(substitute("a ${WS_HOST_app", &vars), "a ${WS_HOST_app");
    }

    #[test]
    fn ws_vars_maps_http_to_host_and_tcp_to_port() {
        let rc = repo_with(&[
            ("app", svc(Some(HttpDecl::Subdomain("api".into())), 80)),
            ("admin", svc(Some(HttpDecl::Enabled(true)), 8080)),
            ("mysql", svc(None, 3306)),
        ]);
        let mut ports = BTreeMap::new();
        ports.insert("mysql".to_string(), 54321u16);

        // container repo (host = false): HTTP services get no WS_PORT.
        let vars = ws_vars(&services(&rc), "feat-x", Some("myapp.test"), &ports, false);
        assert_eq!(vars["WS_HOST_app"], "feat-x.api.myapp.test");
        assert_eq!(vars["WS_HOST_admin"], "feat-x.admin.myapp.test");
        assert_eq!(vars["WS_PORT_mysql"], "54321");
        assert!(!vars.contains_key("WS_PORT_app"));
    }

    #[test]
    fn ws_vars_host_repo_gives_http_service_both_host_and_port() {
        let rc = repo_with(&[("app", svc(Some(HttpDecl::Subdomain("api".into())), 8080))]);
        let mut ports = BTreeMap::new();
        ports.insert("app".to_string(), 20001u16);
        // host = true: an HTTP service gets WS_HOST *and* the port to bind.
        let vars = ws_vars(&services(&rc), "feat-x", Some("myapp.test"), &ports, true);
        assert_eq!(vars["WS_HOST_app"], "feat-x.api.myapp.test");
        assert_eq!(vars["WS_PORT_app"], "20001");
    }

    #[test]
    fn ws_vars_container_http_ignores_stale_port() {
        // A now-HTTP service with a lingering .ws-meta port must NOT leak WS_PORT
        // on a container repo (host = false).
        let rc = repo_with(&[("web", svc(Some(HttpDecl::Subdomain("web".into())), 80))]);
        let mut ports = BTreeMap::new();
        ports.insert("web".to_string(), 20001u16); // stale allocation from a raw-TCP era
        let vars = ws_vars(&services(&rc), "feat-x", Some("myapp.test"), &ports, false);
        assert!(!vars.contains_key("WS_PORT_web"), "stale port must not leak: {vars:?}");
    }

    #[test]
    fn ws_vars_skips_host_without_domain() {
        let rc = repo_with(&[("app", svc(Some(HttpDecl::Subdomain("api".into())), 80))]);
        let vars = ws_vars(&services(&rc), "feat-x", None, &BTreeMap::new(), false);
        assert!(vars.is_empty(), "no domain ⇒ no WS_HOST var: {vars:?}");
    }

    #[test]
    fn override_path_at_is_absolute_for_a_relative_remote_base() {
        // A remote workspace with the default (home-relative) workspace_path must
        // produce an ABSOLUTE server override path, else `COMPOSE_FILE` would
        // double-resolve under the `cd`'d worktree and the remote `up` would fail.
        let cfg = WsConfig {
            remote: Some(crate::config::RemoteConfig::new("srv".into(), Some("myapp".into()))),
            ..Default::default()
        };
        let site = Site::Remote { connection: "srv".into(), home: "/home/u".into() };
        let p = override_path_at(&cfg, &site, Path::new("/local/myapp"), "feat-x", "api");
        assert_eq!(p, "/home/u/myapp/.ws-meta/feat-x-api.override.yml");

        // Local: under the host `.ws-meta`, unchanged.
        let local = WsConfig::default();
        let lp = override_path_at(&local, &Site::Local, Path::new("/ws"), "feat-x", "api");
        assert!(lp.ends_with(".ws-meta/feat-x-api.override.yml"), "{lp}");
    }

    #[test]
    fn declared_env_substitutes_declared_mappings() {
        let mut rc = repo_with(&[("app", svc(Some(HttpDecl::Subdomain("api".into())), 80))]);
        rc.env.insert("APP_URL".into(), "https://${WS_HOST_app}".into());
        let mut vars = BTreeMap::new();
        vars.insert("WS_HOST_app".to_string(), "feat-x.api.myapp.test".to_string());

        let pairs = declared_env(&rc, &vars);
        let app_url = pairs.iter().find(|(k, _)| k == "APP_URL").unwrap();
        assert_eq!(app_url.1, "https://feat-x.api.myapp.test");
    }
}
