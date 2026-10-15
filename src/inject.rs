//! Config injection: per-worktree `DRUP_HOST_*`/`DRUP_PORT_*` values reach the app
//! via env, CLI-arg passthrough, and `${DRUP_*}` template substitution, all driven
//! by [`prepare`]. Substitution is hand-rolled rather than via the `subst` crate
//! so bare `$VAR`s in nginx.conf/redis.conf are copied verbatim.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::{Component, Path};

use crate::compose;
use crate::config::{RepoConfig, RepoKind, ServiceConfig, WsConfig};
use crate::git;
use crate::ports;
use crate::remote::{self, Site};

/// Lifecycle phase; picks port allocation and template rendering so callers
/// state intent, not mechanics.
#[derive(Clone, Copy)]
pub enum Phase {
    /// `up` / `post-create`: allocate ports and render templates.
    Activate,
    /// `drup exec` locally: allocate ports but don't render — exec is a command
    /// runner, not a config step.
    Exec,
    /// `down` / `post-remove` / `status` / remote exec: never allocate or render.
    Inspect,
}

impl Phase {
    fn allocates(self) -> bool {
        matches!(self, Phase::Activate | Phase::Exec)
    }
    fn renders(self) -> bool {
        matches!(self, Phase::Activate)
    }
    /// Deriving shells out to the runtime — too heavy for read-only paths, which
    /// use the declared `services` plus already-recorded ports instead.
    fn derives(self) -> bool {
        matches!(self, Phase::Activate)
    }
    /// Named separately from `renders`/`derives` so read/teardown phases reuse
    /// the override file the last `up` wrote instead of rewriting it.
    fn writes_override(self) -> bool {
        matches!(self, Phase::Activate)
    }
}

/// Compute the injection env for a repo's worktree; `site` decides where derivation, port probes, and the compose override run, while `worktree` is always the local (synced) path used for file checks and template rendering. A bare-name entry is treated as an object with no overrides (see `repo_config`), not an opt-out.
pub fn prepare(
    cfg: &WsConfig,
    site: &Site,
    root: &Path,
    worktree: &Path,
    branch_sanitized: &str,
    repo: &str,
    phase: Phase,
) -> Result<Vec<(String, String)>> {
    // Emitted even for a repo that isn't in this workspace at all, which returns early below.
    let context = workspace_context(cfg);
    let Some(rc) = repo_config(cfg, repo) else { return Ok(context) };

    let site_worktree = site_worktree_path(cfg, site, root, worktree, branch_sanitized, repo);

    // Read-only phases prefer the set recorded at the last `up` — it covers
    // auto-derived services — falling back to declared for a pre-record worktree.
    let (services, all_services): (BTreeMap<String, ServiceConfig>, std::collections::BTreeSet<String>) =
        if phase.derives() {
            effective_services(cfg, site, &site_worktree, worktree, repo, rc)?
        } else {
            let recorded = ports::services(root, branch_sanitized, repo);
            let s = if recorded.is_empty() { rc.services.clone() } else { recorded };
            let names = s.keys().cloned().collect();
            (s, names)
        };

    // The record + lock stay host-side; only the freeness probe follows `site`.
    let host = rc.is_host();
    let port_map = if phase.allocates() {
        // Host repos allocate a port for HTTP services too: N host processes
        // can't share a port the way N containers can, so each needs one to bind.
        let needs: Vec<String> = services
            .iter()
            .filter(|(_, s)| host || !s.is_http())
            .map(|(n, _)| n.clone())
            .collect();
        ports::resolve(site, root, branch_sanitized, repo, &needs)?
    } else {
        ports::load(root, branch_sanitized, repo)
    };

    // Recorded at `up` so read-only phases can reconstruct auto-derived services.
    if phase.derives() {
        ports::record_services(root, branch_sanitized, repo, &services)?;
    }

    let vars = ws_vars(&services, branch_sanitized, cfg.domain.as_deref(), &port_map, host);

    // Peer keys carry a `.` (invalid env-var char) so they're substitution-only,
    // never injected; an unbuilt peer just leaves the token literal.
    let mut subst = vars.clone();
    // Substitutable so `.drup.json` can map `"DOCKER_COMMAND": "${DRUP_RUNTIME}"`
    // instead of teaching the project about drup.
    subst.extend(context.iter().cloned());
    for peer in cfg.repos.iter().map(|r| r.name()).filter(|&n| n != repo) {
        subst.extend(peer_vars(cfg, site, root, branch_sanitized, peer, phase)?);
    }

    if phase.renders() {
        render_all(worktree, rc, &subst)?;
    }

    let declared = declared_env(rc, &subst);
    let mut env: Vec<(String, String)> =
        vars.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    env.extend(declared.iter().cloned());

    // Container repos get a generated override + `COMPOSE_*` namespacing so N
    // parallel worktrees de-conflict raw-TCP host ports.
    if !host && rc.is_container(|| compose::has_compose_file(worktree)) {
        let project = compose::project_name(branch_sanitized, repo);
        // Skipped when routing is off, else referencing the external `drup-net`
        // would fail `compose up` for nothing.
        let join_drup_net = cfg.routing_active();
        let base = base_files_at(worktree, &site_worktree);
        let override_path = override_path_at(cfg, site, root, branch_sanitized, repo);
        env.extend(compose_env(
            site, &base, &override_path, &project, &services, &all_services, &port_map, &declared,
            phase, join_drup_net,
        )?);
    }
    env.extend(context);
    Ok(env)
}

/// Treats a bare-name entry as the object entry with no overrides that it means; before this, `prepare` returned early for such repos, so a bare-name repo with a compose file got the default `compose up -d` lifecycle without `COMPOSE_PROJECT_NAME`, and every branch's compose stack shared one project (the second `up` stole the first's containers, `drup rm` wiped the sibling's DB volumes).
fn repo_config<'a>(cfg: &'a WsConfig, repo: &str) -> Option<&'a RepoConfig> {
    let entry = cfg.repos.iter().find(|r| r.name() == repo)?;
    match entry.config() {
        Some(rc) => Some(rc),
        None => {
            static BARE: std::sync::OnceLock<RepoConfig> = std::sync::OnceLock::new();
            Some(BARE.get_or_init(RepoConfig::default))
        }
    }
}

/// Prefer the effective set recorded at the last `up` — it covers services auto-derived from compose — falling back to declared for a worktree that predates the record.
fn recorded_or_declared(
    root: &Path,
    branch_sanitized: &str,
    repo: &str,
    rc: &RepoConfig,
) -> BTreeMap<String, ServiceConfig> {
    let recorded = ports::services(root, branch_sanitized, repo);
    if recorded.is_empty() { rc.services.clone() } else { recorded }
}

/// `DRUP_REMOTE` reflects the *workspace*, not the current site, so a hook reads one consistent answer wherever it runs. Not named `DRUP_REMOTE_HOST` because that is an input override a nested `drup` call would silently inherit.
fn workspace_context(cfg: &WsConfig) -> Vec<(String, String)> {
    vec![
        ("DRUP_RUNTIME".to_string(), cfg.runtime().to_string()),
        ("DRUP_REMOTE".to_string(), if cfg.remote.is_some() { "1" } else { "0" }.to_string()),
        (
            "DRUP_CONNECTION".to_string(),
            cfg.remote.as_ref().map(|r| r.connection.clone()).unwrap_or_default(),
        ),
    ]
}

/// The worktree path at the execution site — server-side synced dir for a
/// remote workspace, local path otherwise.
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
        // Absolute so `COMPOSE_FILE` in the `cd`'d up script won't double-resolve.
        Some(r) => {
            remote::worktree_path(&site.abs_base(&r.worktree_base(root)), branch_sanitized, repo)
        }
    }
}

/// Base compose files re-homed under `site_worktree` (no-op for a local site).
fn base_files_at(worktree: &Path, site_worktree: &str) -> Vec<String> {
    let base = site_worktree.trim_end_matches('/');
    compose::base_files(worktree)
        .iter()
        .filter_map(|p| p.file_name().map(|n| format!("{base}/{}", n.to_string_lossy())))
        .collect()
}

/// `pub(crate)` so teardown can remove the server-side file it wrote.
pub(crate) fn override_path_at(
    cfg: &WsConfig,
    site: &Site,
    root: &Path,
    branch_sanitized: &str,
    repo: &str,
) -> String {
    match &cfg.remote {
        None => ports::override_file(root, branch_sanitized, repo).to_string_lossy().into_owned(),
        // Absolute, same reason as `site_worktree_path`.
        Some(r) => format!(
            "{}/.drup-meta/{}",
            site.abs_base(&r.worktree_base(root)).trim_end_matches('/'),
            ports::override_filename(branch_sanitized, repo),
        ),
    }
}

/// `COMPOSE_PROJECT_NAME` is set every phase (compose's default is the worktree
/// dir basename, which collides across branches). `COMPOSE_FILE` is set only on
/// activating phases so read/teardown/exec don't reference a possibly-stale
/// override left over from an edited compose file.
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
    join_drup_net: bool,
) -> Result<Vec<(String, String)>> {
    if base.is_empty() {
        return Ok(Vec::new());
    }

    let mut env = vec![("COMPOSE_PROJECT_NAME".to_string(), project.to_string())];
    if phase.writes_override()
        && compose::write_override(site, override_path, services, all_services, port_map, declared, join_drup_net)?
    {
        let mut files: Vec<String> = base.to_vec();
        files.push(override_path.to_string());
        env.push(("COMPOSE_FILE".to_string(), files.join(":")));
    }
    Ok(env)
}

/// Read-only: uses the service set recorded at the last `up` so `down`/`rm` resolve the same route `@id`s `up` created; never allocates or derives.
pub fn routes(
    cfg: &WsConfig,
    root: &Path,
    branch_sanitized: &str,
    repo: &str,
) -> Vec<crate::caddy::RouteSpec> {
    let (Some(rc), Some(domain)) = (repo_config(cfg, repo), cfg.domain.as_deref()) else {
        return Vec::new();
    };
    // Recorded set covers auto-derived services; declared is the pre-record fallback.
    let recorded = ports::services(root, branch_sanitized, repo);
    let services = if recorded.is_empty() { rc.services.clone() } else { recorded };
    let host = rc.is_host();
    let port_map = if host { ports::load(root, branch_sanitized, repo) } else { BTreeMap::new() };
    let project = if host { String::new() } else { compose::project_name(branch_sanitized, repo) };

    route_specs(&services, &port_map, host, &project, branch_sanitized, repo, domain)
}

/// Pure core of [`routes`]; a host service without an allocated port is skipped.
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
        let Some(sub) = svc.subdomain(name) else { continue };
        let hostname = ws_host(branch_sanitized, sub, domain);
        let upstream = if host {
            match port_map.get(name) {
                Some(port) => format!("{}:{port}", crate::caddy::HOST_ALIAS),
                None => continue,
            }
        } else {
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

/// Container-derived services with declared overlaid on top; falls back to
/// declared alone. A failed derive is fatal only for explicit `kind: container`
/// — an inferred one shouldn't hard-fail just because the runtime is out.
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
    Ok((rc.services.clone(), rc.services.keys().cloned().collect()))
}

/// Shared by the `DRUP_HOST_*` env var and the Caddy route match so the two
/// can't drift.
pub(crate) fn ws_host(branch_sanitized: &str, subdomain: &str, domain: &str) -> String {
    format!("{}.{}.{domain}", dns_label(branch_sanitized), dns_label(subdomain))
}

/// One label so a dotted branch can't be suffix-captured by a sibling's
/// wildcard route.
pub(crate) fn dns_label(s: &str) -> String {
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
                vars.insert(format!("DRUP_HOST_{name}"), ws_host(branch_sanitized, sub, d));
            }
        }
        if host || !svc.is_http() {
            if let Some(port) = port_map.get(name) {
                vars.insert(format!("DRUP_PORT_{name}"), port.to_string());
            }
        }
    }
    vars
}

/// A peer repo's vars re-keyed under a `<peer>.` prefix for substitution.
fn peer_vars(
    cfg: &WsConfig,
    site: &Site,
    root: &Path,
    branch_sanitized: &str,
    peer: &str,
    phase: Phase,
) -> Result<BTreeMap<String, String>> {
    let Some(rc) = repo_config(cfg, peer) else { return Ok(BTreeMap::new()) };
    let services = recorded_or_declared(root, branch_sanitized, peer, rc);
    let peer_host = rc.is_host();

    // Allocating phases pre-reserve the peer's ports so `${DRUP_PORT_<peer>.<svc>}`
    // resolves even when the peer worktree hasn't been built yet; the reservation
    // lands in the peer's own `.drup-meta` and its later `up` reuses it.
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
                .strip_prefix("DRUP_HOST_")
                .map(|svc| format!("DRUP_HOST_{peer}.{svc}"))
                .or_else(|| k.strip_prefix("DRUP_PORT_").map(|svc| format!("DRUP_PORT_{peer}.{svc}")))?;
            Some((rekeyed, v))
        })
        .collect())
}

/// Maps drup's `DRUP_*` values onto the app's own var names.
fn declared_env(rc: &RepoConfig, vars: &BTreeMap<String, String>) -> Vec<(String, String)> {
    rc.env.iter().map(|(k, t)| (k.clone(), substitute(t, vars))).collect()
}

/// Registers every dest in `info/exclude` in one pass (one `git` spawn).
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

/// `.drup.json` arrives with a clone, so `dest` is attacker-controlled: without this, an entry like `../../../../.ssh/authorized_keys` had `drup new` and every `drup up` write anywhere the user could. Same rule `copy` entries have always had (`ops::new::seed_copies`); the `is_tracked` guard below inverts for escaping paths and never caught this.
fn ensure_within_worktree(worktree: &Path, field: &str, p: &str) -> Result<()> {
    let rel = Path::new(p);
    if rel.is_absolute() {
        bail!(
            "template {field} `{p}` must be a path inside the worktree — \
             an absolute path would write outside it"
        );
    }
    if rel.components().any(|c| c == Component::ParentDir) {
        bail!("template {field} `{p}` must not contain `..` — it would escape the worktree");
    }

    // Lexically clean, but a committed symlink on the way still lands outside; canonicalise the deepest existing ancestor and require it under the worktree.
    let root = worktree
        .canonicalize()
        .with_context(|| format!("canonicalize worktree {}", worktree.display()))?;
    let joined = worktree.join(rel);
    let mut probe = joined.as_path();
    loop {
        match probe.canonicalize() {
            Ok(real) => {
                if !real.starts_with(&root) {
                    bail!(
                        "template {field} `{p}` resolves to {} — outside the worktree {}",
                        real.display(),
                        root.display()
                    );
                }
                return Ok(());
            }
            Err(_) => match probe.parent() {
                Some(parent) => probe = parent,
                None => return Ok(()),
            },
        }
    }
}

/// Refuses if `dest` is tracked — rendering would dirty a committed file.
fn render_one(
    worktree: &Path,
    src: &str,
    dest: &str,
    vars: &BTreeMap<String, String>,
) -> Result<()> {
    // Before anything touches the filesystem: both fields are attacker-controlled
    // config, and both are joined onto the worktree below.
    ensure_within_worktree(worktree, "src", src)?;
    ensure_within_worktree(worktree, "dest", dest)?;
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

/// Only names present in `vars` (only ever `DRUP_*`) are replaced; every other
/// `$…` is copied byte-for-byte so nginx-style `$host` survives untouched.
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
            worktree_excludes: Vec::new(),
            copy: Vec::new(),
        }
    }

    fn services(rc: &RepoConfig) -> BTreeMap<String, ServiceConfig> {
        rc.services.clone()
    }

    #[test]
    fn workspace_context_reports_runtime_and_remoteness() {
        use crate::config::{RemoteConfig, WsConfig};

        let local = WsConfig::default();
        let ctx = workspace_context(&local);
        let detected = crate::config::detect_runtime().unwrap_or("podman").to_string();
        assert!(ctx.contains(&("DRUP_RUNTIME".to_string(), detected)), "{ctx:?}");
        assert!(ctx.contains(&("DRUP_REMOTE".to_string(), "0".to_string())), "{ctx:?}");
        // Always present so a hook can test it without an unset-variable guard.
        assert!(ctx.contains(&("DRUP_CONNECTION".to_string(), String::new())), "{ctx:?}");

        let remote = WsConfig {
            runtime: Some("docker".to_string()),
            remote: Some(RemoteConfig::new("srv".to_string(), None)),
            ..WsConfig::default()
        };
        let ctx = workspace_context(&remote);
        assert!(ctx.contains(&("DRUP_RUNTIME".to_string(), "docker".to_string())), "{ctx:?}");
        assert!(ctx.contains(&("DRUP_REMOTE".to_string(), "1".to_string())), "{ctx:?}");
        assert!(ctx.contains(&("DRUP_CONNECTION".to_string(), "srv".to_string())), "{ctx:?}");
    }

    #[test]
    fn a_bare_name_compose_repo_gets_a_namespaced_compose_project() {
        // A bare-name entry (`{"repos":["api"]}`) whose worktree has a compose
        // file still runs the compose lifecycle (`is_container_repo` infers it, so
        // `hooks::resolve` hands back `<runtime> compose up -d`). It must therefore
        // get `COMPOSE_PROJECT_NAME`: without it compose falls back to the worktree
        // dir's basename `api`, the same for every branch, and the second
        // worktree's `up` adopts the first's containers/network/volumes.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let wt = root.join(".worktrees/feat-a/api");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join("docker-compose.yml"), "services: {}\n").unwrap();

        let cfg: WsConfig = serde_json::from_str(r#"{"repos":["api"]}"#).unwrap();
        // Inspect: no derive, no allocation, no runtime call — the project name is
        // set on every phase, so this exercises the fix without a container runtime.
        let env = prepare(&cfg, &Site::Local, root, &wt, "feat-a", "api", Phase::Inspect).unwrap();
        let proj = env.iter().find(|(k, _)| k == "COMPOSE_PROJECT_NAME").map(|(_, v)| v.as_str());
        assert_eq!(proj, Some("feat-a-api"), "bare-name compose repo must be namespaced: {env:?}");

        // A name that isn't a repo here still gets only the workspace context.
        let none = prepare(&cfg, &Site::Local, root, &wt, "feat-a", "nope", Phase::Inspect).unwrap();
        assert!(!none.iter().any(|(k, _)| k == "COMPOSE_PROJECT_NAME"), "{none:?}");
    }

    #[test]
    fn a_bare_name_repo_routes_its_recorded_services() {
        // Same rule on the routing side: `prepare` hands a bare-name repo's HTTP
        // services a `DRUP_HOST_*`, so `routes` must produce the Caddy route that
        // hostname resolves to — otherwise the injected host is a dead name.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cfg: WsConfig =
            serde_json::from_str(r#"{"domain":"myapp.test","repos":["api"]}"#).unwrap();
        let recorded = services(&repo_with(&[("web", svc(Some(HttpDecl::Enabled(true)), 80))]));
        ports::record_services(root, "feat-a", "api", &recorded).unwrap();

        let specs = routes(&cfg, root, "feat-a", "api");
        assert_eq!(
            specs.len(),
            1,
            "bare-name repo must still route: {:?}",
            specs.iter().map(|r| &r.id).collect::<Vec<_>>()
        );
        assert_eq!(specs[0].hostname, "feat-a.web.myapp.test");
        assert_eq!(specs[0].upstream, "feat-a-api-web-1:80");
    }

    #[test]
    fn substitute_resolves_a_cross_repo_token() {
        let mut vars = BTreeMap::new();
        vars.insert("DRUP_PORT_app.web".to_string(), "20008".to_string());
        assert_eq!(
            substitute("https://myapp.localhost:${DRUP_PORT_app.web}/x", &vars),
            "https://myapp.localhost:20008/x"
        );
        assert_eq!(
            substitute("${DRUP_PORT_missing.web}", &BTreeMap::new()),
            "${DRUP_PORT_missing.web}"
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
        std::fs::create_dir_all(crate::ports::meta_dir(root)).unwrap();
        std::fs::write(
            crate::ports::meta_dir(root).join("4_feat-3_app.json"),
            r#"{"ports":{"web":20008}}"#,
        )
        .unwrap();

        let pv = peer_vars(&cfg, &Site::Local, root, "feat", "app", Phase::Inspect).unwrap();
        assert_eq!(pv.get("DRUP_PORT_app.web").map(String::as_str), Some("20008"));
        assert!(!pv.contains_key("DRUP_HOST_app.web"));
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
        assert!(peer_vars(&cfg, &Site::Local, root, "feat", "app", Phase::Inspect).unwrap().is_empty());
        assert!(peer_vars(&cfg, &Site::Local, root, "feat", "nope", Phase::Activate).unwrap().is_empty());
    }

    #[test]
    fn peer_vars_pre_allocates_an_unbuilt_peer_on_an_allocating_phase() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cfg: WsConfig = serde_json::from_str(
            r#"{"repos":[{"name":"api","kind":"container"},
                         {"name":"app","kind":"host","services":{"web":{"port":8000}}}]}"#,
        )
        .unwrap();

        let pv = peer_vars(&cfg, &Site::Local, root, "feat", "app", Phase::Activate).unwrap();
        let port = pv.get("DRUP_PORT_app.web").expect("peer port pre-allocated");
        assert!(port.parse::<u16>().is_ok(), "a real port, got {port}");
        // Lands in the peer's own `.drup-meta` so app reuses it when built.
        assert_eq!(
            ports::load(root, "feat", "app").get("web").map(|p| p.to_string()).as_deref(),
            Some(port.as_str()),
        );
    }

    #[test]
    fn route_specs_container_upstreams_and_http_filter() {
        let services = services(&repo_with(&[
            ("web", svc(Some(HttpDecl::Subdomain("web".into())), 80)),
            ("admin", svc(Some(HttpDecl::Enabled(true)), 8080)),
            ("mysql", svc(None, 3306)),
        ]));
        let specs = route_specs(
            &services, &BTreeMap::new(), false, "feat-x-api", "feat-x", "api", "myapp.test",
        );
        assert_eq!(specs.len(), 2, "only HTTP services route: {:?}", specs.iter().map(|r| &r.id).collect::<Vec<_>>());
        let web = specs.iter().find(|r| r.id == "drup-6_feat-x-3_api-3_web").unwrap();
        assert_eq!(web.hostname, "feat-x.web.myapp.test");
        assert_eq!(web.upstream, "feat-x-api-web-1:80");
        let admin = specs.iter().find(|r| r.id == "drup-6_feat-x-3_api-5_admin").unwrap();
        assert_eq!(admin.hostname, "feat-x.admin.myapp.test");
        assert_eq!(admin.upstream, "feat-x-api-admin-1:8080");
    }

    #[test]
    fn route_specs_host_upstreams_and_drop_when_no_port() {
        let services = services(&repo_with(&[
            ("web", svc(Some(HttpDecl::Subdomain("web".into())), 8080)),
            ("api", svc(Some(HttpDecl::Enabled(true)), 9000)),
        ]));
        let ports = [("web".to_string(), 20001u16)].into_iter().collect();
        let specs = route_specs(&services, &ports, true, "", "feat-x", "svc", "myapp.test");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].id, "drup-6_feat-x-3_svc-3_web");
        assert_eq!(specs[0].upstream, "host.containers.internal:20001");
        assert!(!specs[0].wildcard, "web is not a wildcard service");
    }

    #[test]
    fn a_wildcard_service_carries_the_flag_into_its_route_spec() {
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
        // `wildcard` is HTTP-only; the flag must be a silent no-op on raw-TCP,
        // not a hostname-less or bogus route.
        let mut db = svc(None, 5432);
        db.wildcard = true;
        let services = services(&repo_with(&[("db", db)]));
        let ports = [("db".to_string(), 20009u16)].into_iter().collect();
        assert!(route_specs(&services, &ports, false, "proj", "feat-x", "api", "myapp.test").is_empty());
    }

    #[test]
    fn a_dotted_branch_collapses_to_one_hostname_label() {
        // Extra hostname labels would let a sibling branch's wildcard suffix-
        // capture this worktree's traffic.
        assert_eq!(ws_host("v2.1", "web", "myapp.test"), "v2-1.web.myapp.test");
        assert_eq!(ws_host("release-1.2", "a.b", "d.test"), "release-1-2.a-b.d.test");
    }

    #[test]
    fn substitute_only_touches_known_ws_braces() {
        let mut vars = BTreeMap::new();
        vars.insert("DRUP_HOST_app".to_string(), "feat.api.myapp.test".to_string());
        vars.insert("DRUP_PORT_mysql".to_string(), "54321".to_string());

        assert_eq!(
            substitute("https://${DRUP_HOST_app}/x", &vars),
            "https://feat.api.myapp.test/x"
        );
        assert_eq!(substitute("${DRUP_typo}", &vars), "${DRUP_typo}");
        assert_eq!(substitute("$DRUP_HOST_app", &vars), "$DRUP_HOST_app");
        assert_eq!(
            substitute("listen $host; port ${DRUP_PORT_mysql};", &vars),
            "listen $host; port 54321;"
        );
        assert_eq!(substitute("a ${DRUP_HOST_app", &vars), "a ${DRUP_HOST_app");
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

        let vars = ws_vars(&services(&rc), "feat-x", Some("myapp.test"), &ports, false);
        assert_eq!(vars["DRUP_HOST_app"], "feat-x.api.myapp.test");
        assert_eq!(vars["DRUP_HOST_admin"], "feat-x.admin.myapp.test");
        assert_eq!(vars["DRUP_PORT_mysql"], "54321");
        assert!(!vars.contains_key("DRUP_PORT_app"));
    }

    #[test]
    fn ws_vars_host_repo_gives_http_service_both_host_and_port() {
        let rc = repo_with(&[("app", svc(Some(HttpDecl::Subdomain("api".into())), 8080))]);
        let mut ports = BTreeMap::new();
        ports.insert("app".to_string(), 20001u16);
        let vars = ws_vars(&services(&rc), "feat-x", Some("myapp.test"), &ports, true);
        assert_eq!(vars["DRUP_HOST_app"], "feat-x.api.myapp.test");
        assert_eq!(vars["DRUP_PORT_app"], "20001");
    }

    #[test]
    fn ws_vars_container_http_ignores_stale_port() {
        let rc = repo_with(&[("web", svc(Some(HttpDecl::Subdomain("web".into())), 80))]);
        let mut ports = BTreeMap::new();
        ports.insert("web".to_string(), 20001u16); // stale allocation from a raw-TCP era
        let vars = ws_vars(&services(&rc), "feat-x", Some("myapp.test"), &ports, false);
        assert!(!vars.contains_key("DRUP_PORT_web"), "stale port must not leak: {vars:?}");
    }

    #[test]
    fn ws_vars_skips_host_without_domain() {
        let rc = repo_with(&[("app", svc(Some(HttpDecl::Subdomain("api".into())), 80))]);
        let vars = ws_vars(&services(&rc), "feat-x", None, &BTreeMap::new(), false);
        assert!(vars.is_empty(), "no domain ⇒ no DRUP_HOST var: {vars:?}");
    }

    #[test]
    fn override_path_at_is_absolute_for_a_relative_remote_base() {
        // A relative override path would double-resolve under the `cd`'d worktree.
        let cfg = WsConfig {
            remote: Some(crate::config::RemoteConfig::new("srv".into(), Some("myapp".into()))),
            ..Default::default()
        };
        let site = Site::Remote { connection: "srv".into(), home: "/home/u".into() };
        let p = override_path_at(&cfg, &site, Path::new("/local/myapp"), "feat-x", "api");
        assert_eq!(p, "/home/u/myapp/.drup-meta/6_feat-x-3_api.override.yml");

        let local = WsConfig::default();
        let lp = override_path_at(&local, &Site::Local, Path::new("/ws"), "feat-x", "api");
        assert!(lp.ends_with(".drup-meta/6_feat-x-3_api.override.yml"), "{lp}");
    }

    #[test]
    fn template_paths_must_stay_inside_the_worktree() {
        // `templates` is attacker-controlled config (`.drup.json` ships with a
        // clone), and both fields are joined onto the worktree. Every escape
        // shape is refused; ordinary relative paths are not.
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(wt.join("deploy")).unwrap();

        // Ordinary config keeps working: a plain name, a nested dest, a `./` prefix.
        for ok in ["svc.conf", "deploy/nginx.conf", "./config/generated/svc.conf"] {
            ensure_within_worktree(&wt, "dest", ok).unwrap_or_else(|e| panic!("`{ok}` refused: {e}"));
        }

        // Traversal, in the middle as well as at the front.
        let err = ensure_within_worktree(&wt, "dest", "../../../pwned.txt").unwrap_err().to_string();
        assert!(err.contains("must not contain `..`"), "{err}");
        assert!(ensure_within_worktree(&wt, "src", "deploy/../../escape").is_err());

        // Absolute: `Path::join` would drop the worktree entirely.
        let err = ensure_within_worktree(&wt, "dest", "/etc/cron.d/pwn").unwrap_err().to_string();
        assert!(err.contains("must be a path inside the worktree"), "{err}");

        // Lexically clean but symlinked out — a committed `esc -> /` needs no `..`.
        #[cfg(unix)]
        {
            let outside = tmp.path().join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            std::os::unix::fs::symlink(&outside, wt.join("esc")).unwrap();
            let err = ensure_within_worktree(&wt, "dest", "esc/pwned.txt").unwrap_err().to_string();
            assert!(err.contains("outside the worktree"), "{err}");
        }
    }

    #[test]
    fn declared_env_substitutes_declared_mappings() {
        let mut rc = repo_with(&[("app", svc(Some(HttpDecl::Subdomain("api".into())), 80))]);
        rc.env.insert("APP_URL".into(), "https://${DRUP_HOST_app}".into());
        let mut vars = BTreeMap::new();
        vars.insert("DRUP_HOST_app".to_string(), "feat-x.api.myapp.test".to_string());

        let pairs = declared_env(&rc, &vars);
        let app_url = pairs.iter().find(|(k, _)| k == "APP_URL").unwrap();
        assert_eq!(app_url.1, "https://feat-x.api.myapp.test");
    }
}
