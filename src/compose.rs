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
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
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

/// The base compose file(s) compose would discover in `worktree`, as absolute
/// paths: the primary file (first of `COMPOSE_FILES` present) plus its
/// auto-merged `<family>.override.{yaml,yml}` sibling if one exists. Empty when
/// no compose file is present. These lead the `COMPOSE_FILE` list, ahead of our
/// generated override — once `COMPOSE_FILE` is set, compose no longer
/// auto-discovers, so the base file has to be named for the merge to include it.
///
/// The override sibling is matched to the *primary's family* (`compose.*` →
/// `compose.override.*`, `docker-compose.*` → `docker-compose.override.*`) —
/// compose only merges the matching family, so pairing a `compose.yaml` with a
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

/// A legal compose project name for a worktree: `<branch>-<repo>` lowercased,
/// with every character outside `[a-z0-9_-]` replaced by `-` and a leading run
/// of non-alphanumerics dropped. Compose requires `COMPOSE_PROJECT_NAME` to
/// match `[a-z0-9][a-z0-9_-]*` and rejects anything else, but `sanitize_branch`
/// only strips slashes — so an uppercase or dotted branch (`Feat/JIRA-1.2`)
/// would otherwise break `up` *and* `down`.
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

/// Generate the per-worktree compose override at `path` from the current
/// allocation, or remove a now-stale one. Returns `true` when a file was
/// written (so the caller adds it to `COMPOSE_FILE`), `false` when there was
/// nothing to override.
pub fn write_override(
    path: &Path,
    services: &BTreeMap<String, ServiceConfig>,
    port_map: &BTreeMap<String, u16>,
    env: &[(String, String)],
) -> Result<bool> {
    match render_override(services, port_map, env) {
        Some(content) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(path, content)
                .with_context(|| format!("write {}", path.display()))?;
            Ok(true)
        }
        None => {
            // Drop an override left by a prior `up` that no longer applies, so a
            // later read-only phase can't reference a stale file.
            let _ = std::fs::remove_file(path);
            Ok(false)
        }
    }
}

/// Render the slice-6.1 compose override: a `!override` ports list per raw-TCP
/// service, republishing it on its allocated host port, plus the declared `env`
/// materialized as `environment:` entries (a shell env doesn't cross into a
/// container). `None` when there's nothing to override — no allocated raw-TCP
/// port and no declared env.
///
/// **Deliberately partial (see the build plan):** no `ws-net` join and no
/// `!reset` of HTTP host-ports. An external `ws-net` that doesn't exist yet
/// would break `compose up`, and dropping the HTTP publish is pointless before
/// Caddy routes it — both move to slice 6.2.
fn render_override(
    services: &BTreeMap<String, ServiceConfig>,
    port_map: &BTreeMap<String, u16>,
    env: &[(String, String)],
) -> Option<String> {
    // Compose services that also publish an HTTP port. `ports: !override`
    // replaces a service's *entire* ports list, so reissuing a raw-TCP port on
    // such a service would clobber its HTTP host-publish — which 6.1 must leave
    // intact (Caddy/`!reset` is 6.2). So a service mixing HTTP + raw-TCP ports
    // keeps its base ports untouched; its raw port isn't reissued until 6.2 can
    // reset the HTTP one cleanly. (The common case — a service is purely HTTP or
    // purely raw-TCP — is unaffected.)
    let http_services: BTreeSet<&str> = services
        .iter()
        .filter(|(_, s)| s.is_http())
        .map(|(k, s)| compose_service(k, s.port))
        .collect();

    // Group allocated host ports by real compose service name. A `!override`
    // replaces a service's whole ports list, so a multi-port service's ports
    // must be collected into one entry.
    let mut ports: BTreeMap<String, Vec<(u16, u16)>> = BTreeMap::new();
    for (key, svc) in services {
        if svc.is_http() {
            continue;
        }
        let name = compose_service(key, svc.port);
        if http_services.contains(name) {
            continue;
        }
        if let Some(&host) = port_map.get(key) {
            ports.entry(name.to_string()).or_default().push((host, svc.port));
        }
    }
    if ports.is_empty() && env.is_empty() {
        return None;
    }

    // Services needing an entry: those with a ports override, plus — when
    // there's declared env — every effective compose service (the `env` block
    // is repo-level, so it goes to the whole stack; compose merges it into each
    // service's own `environment`).
    let mut names: BTreeSet<String> = ports.keys().cloned().collect();
    if !env.is_empty() {
        names.extend(services.iter().map(|(k, s)| compose_service(k, s.port).to_string()));
    }

    let mut out = String::from("# Generated by ws — do not edit. Regenerated on `ws up`.\nservices:\n");
    for name in &names {
        out.push_str(&format!("  {name}:\n"));
        if let Some(list) = ports.get(name) {
            out.push_str("    ports: !override\n");
            for (host, internal) in list {
                out.push_str(&format!("      - \"{host}:{internal}\"\n"));
            }
        }
        if !env.is_empty() {
            out.push_str("    environment:\n");
            for (k, v) in env {
                out.push_str(&format!("      {k}: {}\n", yaml_dquote(v)));
            }
        }
    }
    Some(out)
}

/// Recover the real compose service name from a derived service key: a
/// single-port service keeps its bare name, a multi-port one derives as
/// `<svc>_<internal-port>`, so strip a trailing `_<port>`. (A service literally
/// named `<x>_<port>` publishing that port collides here — the same accepted
/// ambiguity `derive` already carries.)
fn compose_service(key: &str, internal_port: u16) -> &str {
    key.strip_suffix(&format!("_{internal_port}")).unwrap_or(key)
}

/// Double-quote a YAML scalar, escaping `\`, `"`, and the control characters
/// that would otherwise break a single-line double-quoted scalar. Env values
/// are normally URLs/ports (the `:`/`/` are safe once quoted), but a declared
/// value could contain a newline/tab; emit those as YAML escapes rather than a
/// raw byte that would corrupt the document.
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
            _ => out.push(c),
        }
    }
    out.push('"');
    out
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

    fn svc(http: Option<HttpDecl>, port: u16) -> ServiceConfig {
        ServiceConfig { http, port, shared: false }
    }

    fn map(entries: &[(&str, ServiceConfig)]) -> BTreeMap<String, ServiceConfig> {
        entries.iter().map(|(n, s)| ((*n).into(), s.clone())).collect()
    }

    #[test]
    fn override_reissues_raw_tcp_ports_and_leaves_http_alone() {
        let services = map(&[
            ("web", svc(Some(HttpDecl::Subdomain("web".into())), 80)),
            ("db", svc(None, 3306)),
        ]);
        let ports = [("db".to_string(), 20001u16)].into_iter().collect();
        let out = render_override(&services, &ports, &[]).expect("some override");

        assert!(out.contains("  db:\n    ports: !override\n      - \"20001:3306\""), "{out}");
        // The HTTP service keeps its host publish in 6.1 (Caddy/`!reset` is 6.2).
        assert!(!out.contains("web:"), "HTTP service must not appear in the override: {out}");
    }

    #[test]
    fn override_groups_multiport_service_under_one_entry() {
        // A multi-port service derives as `<svc>_<port>`; both allocated ports
        // must land in one `!override` list under the real service name.
        let services = map(&[
            ("mailpit_1025", svc(None, 1025)),
            ("mailpit_8025", svc(None, 8025)),
        ]);
        let ports =
            [("mailpit_1025".to_string(), 20002u16), ("mailpit_8025".to_string(), 20003u16)]
                .into_iter()
                .collect();
        let out = render_override(&services, &ports, &[]).expect("some override");

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
        let out = render_override(&services, &ports, &env).expect("some override");

        // Both real compose services get the env (repo-level `env` → whole stack).
        assert_eq!(out.matches("APP_URL:").count(), 2, "{out}");
        // The URL's `:` is safe because the value is double-quoted.
        assert!(out.contains("APP_URL: \"https://feat-x.web.myapp.test\""), "{out}");
    }

    #[test]
    fn override_is_none_when_nothing_to_reissue() {
        // All-HTTP stack, no declared env → nothing for 6.1 to override.
        let services = map(&[("web", svc(Some(HttpDecl::Subdomain("web".into())), 80))]);
        assert!(render_override(&services, &BTreeMap::new(), &[]).is_none());
    }

    #[test]
    fn override_skips_service_that_also_publishes_http() {
        // A service publishing 80 (HTTP) + 9000 (raw) derives as `web` + `web_9000`.
        // Reissuing 9000 via `!override` would wipe the 80 publish, so 6.1 must
        // leave the whole service's ports alone.
        let services = map(&[
            ("web", svc(Some(HttpDecl::Subdomain("web".into())), 80)),
            ("web_9000", svc(None, 9000)),
        ]);
        let ports = [("web_9000".to_string(), 20005u16)].into_iter().collect();
        // No env either → nothing safe to override at all.
        assert!(render_override(&services, &ports, &[]).is_none());
    }

    #[test]
    fn compose_service_keeps_a_legit_underscored_single_port_name() {
        // Single-port service `my_db` on 3306: derived key is the bare name (no
        // `_3306` suffix), so it must NOT be stripped. Only the multi-port
        // `<svc>_<its-own-port>` form is recovered.
        assert_eq!(compose_service("my_db", 3306), "my_db");
        assert_eq!(compose_service("mailpit_8025", 8025), "mailpit");
    }

    #[test]
    fn project_name_is_compose_legal() {
        // Lowercased, `/`-sanitized branch already legal.
        assert_eq!(project_name("feat-foo", "api"), "feat-foo-api");
        // Uppercase + dot (sanitize_branch only strips slashes) → normalized.
        assert_eq!(project_name("Feat-JIRA-1.2", "api"), "feat-jira-1-2-api");
        // A leading non-alphanumeric run is trimmed.
        assert_eq!(project_name("-x", "api"), "x-api");
    }

    #[test]
    fn base_files_ignores_a_mismatched_family_override() {
        // A `compose.yaml` primary must not pick up a `docker-compose.override.yml`
        // (compose's own discovery would never merge that pair).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("compose.yaml"), "services: {}\n").unwrap();
        std::fs::write(dir.path().join("docker-compose.override.yml"), "services: {}\n").unwrap();
        let files = base_files(dir.path());
        assert_eq!(files.len(), 1, "only the primary, no cross-family override: {files:?}");
        assert!(files[0].ends_with("compose.yaml"));
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

        // No compose file → empty.
        assert!(base_files(tempfile::tempdir().unwrap().path()).is_empty());
    }
}
