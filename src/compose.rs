//! Auto-derive a repo's `services` from its compose file.
//!
//! For a container repo, `ws up`/`ws new` runs `<runtime> compose config
//! --format json` in the worktree — the authoritative view, since compose does
//! its own file discovery and applies env interpolation / `extends` / anchors /
//! overrides — and turns each published container port into a `ServiceConfig`.
//! The `.ws.json` `services` block is then overlaid on top (see
//! `inject::effective_services`), so a stale block is never *wrong*, only less
//! customized.
//!
//! **One entry per (service, port).** A compose service can publish several
//! ports (mailpit = SMTP 1025 + UI 8025); each becomes its own derived service —
//! `<svc>` when the service has a single derived port, `<svc>_<port>` (underscore
//! so the `WS_PORT_*` env name stays shell-clean) when it has more than one.
//!
//! **Conservative HTTP classification.** Only container port 80 or 443 is routed
//! as HTTP (`subdomain = service name`); everything else is raw TCP (allocated a
//! host port). Widen via the `.ws.json` override, not here — mis-marking a raw
//! service HTTP would have Caddy try to HTTP-proxy a non-HTTP protocol.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use crate::config::{HttpDecl, ServiceConfig};

/// Compose files `compose config` discovers, in the order it prefers them. Used
/// only to *infer* whether a repo is a container repo when `kind` is unset;
/// `compose config` itself does the real discovery.
const COMPOSE_FILES: &[&str] =
    &["compose.yaml", "compose.yml", "docker-compose.yaml", "docker-compose.yml"];

/// Whether `dir` looks like a compose project (used for `kind` inference).
pub fn has_compose_file(dir: &Path) -> bool {
    COMPOSE_FILES.iter().any(|f| dir.join(f).is_file())
}

/// Derive services from `<runtime> compose config --format json` run in
/// `worktree`. Errors if the runtime is missing, there's no compose file, or the
/// output doesn't parse — callers fall back to the declared `services`.
pub fn derive(runtime: &str, worktree: &Path) -> Result<BTreeMap<String, ServiceConfig>> {
    let out = Command::new(runtime)
        .args(["compose", "config", "--format", "json"])
        .current_dir(worktree)
        .output()
        .with_context(|| format!("spawn `{runtime} compose config`"))?;
    if !out.status.success() {
        anyhow::bail!(
            "`{runtime} compose config` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    parse(&String::from_utf8_lossy(&out.stdout))
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

/// Parse `compose config --format json` output into derived services.
fn parse(json: &str) -> Result<BTreeMap<String, ServiceConfig>> {
    let cfg: ComposeConfig = serde_json::from_str(json).context("parse compose config JSON")?;
    let mut out = BTreeMap::new();
    for (name, svc) in &cfg.services {
        derive_service(name, svc, &mut out);
    }
    Ok(out)
}

/// Expand one compose service into its derived `ServiceConfig` entries.
fn derive_service(name: &str, svc: &ComposeService, out: &mut BTreeMap<String, ServiceConfig>) {
    // TCP target ports, deduped and ordered (BTreeSet), split into HTTP-ish
    // (80/443) and the rest. UDP and anything without a numeric target is ignored.
    let ports: std::collections::BTreeSet<u16> = svc
        .ports
        .iter()
        .filter(|p| p.protocol.as_deref().unwrap_or("tcp") == "tcp")
        .map(|p| p.target)
        .collect();
    let (http, tcp): (Vec<u16>, Vec<u16>) =
        ports.into_iter().partition(|p| *p == 80 || *p == 443);

    // One HTTP entry per service, dialing plain 80 when present (Caddy fronts TLS
    // and proxies to it), else 443.
    if !http.is_empty() {
        let dial = if http.contains(&80) { 80 } else { 443 };
        out.insert(
            name.to_string(),
            ServiceConfig { http: Some(HttpDecl::Subdomain(name.to_string())), port: dial, shared: false },
        );
    }
    // The bare service name is used only when the service yields exactly one
    // entry total; otherwise suffix with the port so keys (and `WS_PORT_*` names)
    // stay distinct.
    let single = http.is_empty() && tcp.len() == 1;
    for p in tcp {
        let key = if single { name.to_string() } else { format!("{name}_{p}") };
        out.insert(key, ServiceConfig { http: None, port: p, shared: false });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Matches the `compose config --format json` shape: services.<name>.ports[]
    // = {target, published?, protocol}.
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

        // web (80+443) → one HTTP entry dialing 80, subdomain = service name.
        let web = &s["web"];
        assert!(matches!(web.http, Some(HttpDecl::Subdomain(ref d)) if d == "web"));
        assert_eq!(web.port, 80);

        // Single raw-TCP ports keep the bare service name.
        assert!(s["db"].http.is_none() && s["db"].port == 3306);
        assert!(s["redis"].http.is_none() && s["redis"].port == 6379);

        // Multi-port service → one entry per port, suffixed, underscore-joined.
        assert_eq!(s["mailpit_1025"].port, 1025);
        assert_eq!(s["mailpit_8025"].port, 8025);
        assert!(s["mailpit_1025"].http.is_none());
        assert!(!s.contains_key("mailpit"), "multi-port must not keep the bare name");

        // `app` publishes nothing (expose-only) → not derived.
        assert!(!s.contains_key("app"));
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
        // web on 80 plus a 9000 metrics port → HTTP `web` + TCP `web_9000`.
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
}
