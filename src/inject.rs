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

use crate::config::{RepoConfig, WsConfig};
use crate::git;
use crate::ports;

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
}

/// Compute the injection env (`WS_HOST_*`/`WS_PORT_*` + declared `env` mappings)
/// for a repo's worktree, allocating host ports and rendering templates as the
/// `phase` dictates. A bare-name repo, or one with no services/env, yields an
/// empty vec. This is the one function every op calls.
pub fn prepare(
    cfg: &WsConfig,
    root: &Path,
    worktree: &Path,
    branch_sanitized: &str,
    repo: &str,
    phase: Phase,
) -> Result<Vec<(String, String)>> {
    let Some(rc) = cfg.repo_config(repo) else { return Ok(Vec::new()) };

    // Host ports are a LOCAL concept: `ws` allocates them on the machine that
    // runs the stack. For a remote workspace the server owns its own ports
    // (allocation there is a later offload slice), so we neither allocate nor
    // load here — injecting a local port number into a server-side command or a
    // synced config file would just be wrong. `WS_HOST_*` is unaffected: the
    // routed hostname is location-transparent, identical local or remote.
    let port_map = if phase.allocates() && cfg.remote.is_none() {
        let needs: Vec<String> = rc
            .services
            .iter()
            .filter(|(_, s)| !s.is_http())
            .map(|(n, _)| n.clone())
            .collect();
        ports::resolve(root, branch_sanitized, repo, &needs)?
    } else if cfg.remote.is_none() {
        ports::load(root, branch_sanitized, repo)
    } else {
        BTreeMap::new()
    };

    let vars = ws_vars(rc, branch_sanitized, cfg.domain.as_deref(), &port_map);
    if phase.renders() {
        render_all(worktree, rc, &vars)?;
    }
    Ok(env_pairs(rc, &vars))
}

/// The `WS_HOST_<svc>` / `WS_PORT_<svc>` variable map. HTTP services yield a
/// hostname (only when a top-level `domain` is set — without one there's no
/// routing target yet); raw-TCP services yield their allocated host port.
fn ws_vars(
    rc: &RepoConfig,
    branch_sanitized: &str,
    domain: Option<&str>,
    port_map: &BTreeMap<String, u16>,
) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();
    for (name, svc) in &rc.services {
        match svc.subdomain(name) {
            Some(sub) => {
                if let Some(d) = domain {
                    vars.insert(format!("WS_HOST_{name}"), format!("{branch_sanitized}.{sub}.{d}"));
                }
            }
            None => {
                if let Some(port) = port_map.get(name) {
                    vars.insert(format!("WS_PORT_{name}"), port.to_string());
                }
            }
        }
    }
    vars
}

/// The final env pairs: every `WS_HOST_*`/`WS_PORT_*` var, then each declared
/// `env` entry with its `${WS_*}` tokens substituted. Declared entries come last
/// so a mapping onto an app var name can reference the vars above it.
fn env_pairs(rc: &RepoConfig, vars: &BTreeMap<String, String>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vars.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    for (key, template) in &rc.env {
        out.push((key.clone(), substitute(template, vars)));
    }
    out
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
        ServiceConfig { http, port, shared: false }
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
        }
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

        let vars = ws_vars(&rc, "feat-x", Some("myapp.test"), &ports);
        assert_eq!(vars["WS_HOST_app"], "feat-x.api.myapp.test");
        assert_eq!(vars["WS_HOST_admin"], "feat-x.admin.myapp.test");
        assert_eq!(vars["WS_PORT_mysql"], "54321");
        assert!(!vars.contains_key("WS_PORT_app"));
    }

    #[test]
    fn ws_vars_skips_host_without_domain() {
        let rc = repo_with(&[("app", svc(Some(HttpDecl::Subdomain("api".into())), 80))]);
        let vars = ws_vars(&rc, "feat-x", None, &BTreeMap::new());
        assert!(vars.is_empty(), "no domain ⇒ no WS_HOST var: {vars:?}");
    }

    #[test]
    fn env_pairs_substitutes_declared_mappings() {
        let mut rc = repo_with(&[("app", svc(Some(HttpDecl::Subdomain("api".into())), 80))]);
        rc.env.insert("APP_URL".into(), "https://${WS_HOST_app}".into());
        let mut vars = BTreeMap::new();
        vars.insert("WS_HOST_app".to_string(), "feat-x.api.myapp.test".to_string());

        let pairs = env_pairs(&rc, &vars);
        let app_url = pairs.iter().find(|(k, _)| k == "APP_URL").unwrap();
        assert_eq!(app_url.1, "https://feat-x.api.myapp.test");
    }
}
