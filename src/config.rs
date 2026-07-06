use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Deserialize, Serialize, Default, schemars::JsonSchema)]
pub struct WsConfig {
    #[serde(skip_serializing_if = "Option::is_none", default, rename = "$schema")]
    pub schema: Option<String>,
    /// Private TLD for per-worktree hostnames (routing), e.g. `myapp.test`.
    /// Location-transparent — applies to local and remote alike — so it lives
    /// top-level, not under `remote`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub domain: Option<String>,
    /// Container runtime for `ws`'s own calls (compose introspection, the shared
    /// network, the Caddy container). `"podman"` | `"docker"`; default `"podman"`.
    /// Also top-level: routing is location-transparent, not a remote-only concern.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub runtime: Option<String>,
    /// Remote-offload block. Absent → the workspace runs locally. `.ws.json` is
    /// itself per-machine (it describes your local multi-repo layout, not a
    /// shared project config), so the remote target lives here directly — no
    /// separate per-machine overlay file.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub remote: Option<RemoteConfig>,
    #[serde(default)]
    pub repos: Vec<RepoEntry>,
}

/// v2 remote-offload block. When present, every repo's stack runs on the
/// server and location-transparent ops (`ws exec`, later `up`/`down`) target it.
/// `runtime` and `domain` are NOT here — they're top-level, because routing
/// applies to local workspaces too (see `WsConfig`).
#[derive(Deserialize, Serialize, schemars::JsonSchema)]
pub struct RemoteConfig {
    /// SSH host to reach the server — a Tailscale MagicDNS name or `~/.ssh/config` alias.
    pub connection: String,
    /// Parent dir on the server holding the synced worktrees. Relative paths are
    /// resolved against the SSH login home; absolute paths are used verbatim.
    /// Omitted → defaults to the workspace's own directory name (so two projects
    /// on one server don't collide). See `worktree_base`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_path: Option<String>,
}

impl RemoteConfig {
    /// Construct a remote block.
    pub fn new(connection: String, workspace_path: Option<String>) -> Self {
        Self { connection, workspace_path }
    }

    /// The server-side parent dir for this workspace's synced worktrees:
    /// the configured `workspace_path`, or the workspace's own directory name
    /// as a home-relative default.
    pub fn worktree_base(&self, ws_root: &Path) -> String {
        self.workspace_path.clone().unwrap_or_else(|| {
            ws_root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "workspace".to_string())
        })
    }
}

/// A repo entry is either a bare name (convention-only) or an object with overrides.
#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum RepoEntry {
    Name(String),
    Extended(RepoConfig),
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
pub struct RepoConfig {
    pub name: String,
    /// How `ws` launches and de-conflicts this repo's stack. `None` → inferred
    /// (a compose file present ⇒ `Container`, else `Host`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<RepoKind>,
    /// Routing + per-service ports. For a compose repo this is auto-derived from
    /// `<runtime> compose config` at `ws up` and this block is an optional
    /// override layer; for a host repo it's required (nothing to introspect).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub services: HashMap<String, ServiceConfig>,
    /// Declarative lifecycle commands, keyed by event (`post-create`, `up`,
    /// `down`, `post-remove`, `status`). Each is one command or an array of
    /// commands, run via `sh -c` with the `WS_*` env forwarded. A value may be a
    /// path to a script file for complex logic. Supersedes `.ws-hooks/`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub scripts: HashMap<String, Script>,
    /// Explicit env-var injection: your app's var name → a `${WS_*}` template.
    /// `ws` owns `WS_HOST_*`/`WS_PORT_*`; you declare which of your vars get them.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
    /// Format-blind config templates (the escape hatch): a committed `src`
    /// rendered to a gitignored `dest` with `${WS_*}` tokens substituted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub templates: Vec<Template>,
    /// Legacy per-event hook path overrides. Keys are event names. Values are
    /// paths relative to the config dir. Deprecated by `scripts`; kept until the
    /// execution path migrates.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub hooks: HashMap<String, String>,
    /// Per-worktree git-exclude entries appended to <wt>/.git/info/exclude.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub worktree_excludes: Vec<String>,
}

/// How a repo's stack is launched and its ports de-conflicted.
#[derive(Deserialize, Serialize, schemars::JsonSchema, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum RepoKind {
    /// Compose stack. N copies de-conflicted via a generated compose override;
    /// supervised by the runtime.
    Container,
    /// Host process (e.g. `cargo run`). Ports de-conflicted via the `bind(2)`
    /// shim; supervised as a `systemd --user` transient unit.
    Host,
}

/// A single service within a repo: its port and whether it's HTTP-routed.
#[derive(Deserialize, Serialize, schemars::JsonSchema)]
pub struct ServiceConfig {
    /// HTTP routing. A string is the subdomain (`"api"` → `<branch>.api.<domain>`);
    /// `true` routes it using the service name as the subdomain; absent or `false`
    /// means raw TCP — `ws` allocates a host port and exposes `WS_PORT_<service>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpDecl>,
    /// The container-internal (or host-process-bound) port for this service.
    pub port: u16,
    /// Run this service once as shared infra instead of per-worktree. Default
    /// false (isolated). Right for redis/ES; wrong for the SQL DB.
    #[serde(default, skip_serializing_if = "is_false")]
    pub shared: bool,
}

/// How a service participates in HTTP routing.
#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum HttpDecl {
    /// Explicit subdomain label.
    Subdomain(String),
    /// `true` → route as HTTP with the service name as the subdomain; `false` → not HTTP.
    Enabled(bool),
}

/// A lifecycle command: one shell command, or several run in order.
#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum Script {
    One(String),
    Many(Vec<String>),
}

impl Script {
    /// The command(s) to run, in order.
    pub fn commands(&self) -> &[String] {
        match self {
            Script::One(s) => std::slice::from_ref(s),
            Script::Many(v) => v,
        }
    }
}

/// A format-blind config template: render `src` → `dest` with `${WS_*}` tokens.
#[derive(Deserialize, Serialize, schemars::JsonSchema)]
pub struct Template {
    pub src: String,
    pub dest: String,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl WsConfig {
    /// Container runtime for `ws`'s own calls, defaulting to `podman`.
    // Consumed by the routing slices (compose introspection / Caddy); parsed now
    // so the schema is stable.
    #[allow(dead_code)]
    pub fn runtime(&self) -> &str {
        self.runtime.as_deref().unwrap_or("podman")
    }

    /// The extended config for `repo`, if it's an object entry (not a bare
    /// name). The single repo→config lookup shared by script/hook resolution.
    pub fn repo_config(&self, repo: &str) -> Option<&RepoConfig> {
        self.repos.iter().find(|r| r.name() == repo).and_then(|r| r.config())
    }
}

impl RepoEntry {
    pub fn name(&self) -> &str {
        match self {
            RepoEntry::Name(s) => s,
            RepoEntry::Extended(c) => &c.name,
        }
    }

    /// The extended config for this repo, if it's an object entry (not a bare name).
    pub fn config(&self) -> Option<&RepoConfig> {
        match self {
            RepoEntry::Name(_) => None,
            RepoEntry::Extended(c) => Some(c),
        }
    }

    pub fn worktree_excludes(&self) -> &[String] {
        match self {
            RepoEntry::Name(_) => &[],
            RepoEntry::Extended(c) => &c.worktree_excludes,
        }
    }
}

/// Load a workspace config from `<workspace-root>/.ws.json`.
pub fn load(root: &Path) -> Result<WsConfig> {
    read_json(&root.join(".ws.json"))
}

fn read_json<T: for<'de> Deserialize<'de>>(p: &Path) -> Result<T> {
    let raw = std::fs::read_to_string(p)
        .with_context(|| format!("read {}", p.display()))?;
    // Strip a leading UTF-8 BOM. Some Windows editors + `git core.autocrlf`
    // configurations materialize one; serde_json otherwise reports the
    // opaque `expected value at line 1 column 1`.
    let trimmed = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    serde_json::from_str(trimmed).with_context(|| format!("parse {}", p.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_json_strips_utf8_bom() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "\u{feff}{\"repos\":[]}").unwrap();
        let cfg: WsConfig = read_json(tmp.path()).unwrap();
        assert!(cfg.repos.is_empty());
    }

    #[test]
    fn parses_top_level_domain_runtime_and_remote() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".ws.json"),
            r#"{"domain":"myapp.test","repos":["api"],"remote":{"connection":"srv","workspace_path":"workspace"}}"#,
        )
        .unwrap();
        let cfg = load(dir.path()).unwrap();
        assert_eq!(cfg.domain.as_deref(), Some("myapp.test"));
        // runtime absent → defaults to podman via the accessor.
        assert_eq!(cfg.runtime(), "podman");
        let remote = cfg.remote.expect("remote block");
        assert_eq!(remote.connection, "srv");
        assert_eq!(remote.workspace_path.as_deref(), Some("workspace"));
    }

    #[test]
    fn runtime_override_is_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".ws.json"), r#"{"runtime":"docker","repos":[]}"#).unwrap();
        assert_eq!(load(dir.path()).unwrap().runtime(), "docker");
    }

    #[test]
    fn parses_extended_repo_with_services_scripts_env_templates() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".ws.json"),
            r#"{"repos":[{
                "name":"api","kind":"container",
                "services":{"app":{"http":"api","port":80},"mysql":{"port":3306},"redis":{"port":6379,"shared":true}},
                "scripts":{"post-create":["composer install","php artisan migrate"],"up":"docker compose up -d"},
                "env":{"APP_URL":"https://${WS_HOST_app}"},
                "templates":[{"src":"nginx.conf.tmpl","dest":"nginx.conf"}]
            }]}"#,
        )
        .unwrap();
        let cfg = load(dir.path()).unwrap();
        let repo = cfg.repos[0].config().expect("extended repo");
        assert_eq!(repo.kind, Some(RepoKind::Container));

        let app = &repo.services["app"];
        assert_eq!(app.port, 80);
        assert!(matches!(app.http, Some(HttpDecl::Subdomain(ref s)) if s == "api"));
        assert!(!app.shared);
        assert!(repo.services["mysql"].http.is_none());
        assert!(repo.services["redis"].shared);

        assert!(matches!(repo.scripts["post-create"], Script::Many(ref v) if v.len() == 2));
        assert!(matches!(repo.scripts["up"], Script::One(ref s) if s == "docker compose up -d"));
        assert_eq!(repo.env["APP_URL"], "https://${WS_HOST_app}");
        assert_eq!(repo.templates[0].src, "nginx.conf.tmpl");
        assert_eq!(repo.templates[0].dest, "nginx.conf");
    }

    #[test]
    fn http_true_and_bare_name_repo() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".ws.json"),
            r#"{"repos":["web",{"name":"api","services":{"app":{"http":true,"port":8080}}}]}"#,
        )
        .unwrap();
        let cfg = load(dir.path()).unwrap();
        // Bare-name entry stays valid and carries no extended config.
        assert_eq!(cfg.repos[0].name(), "web");
        assert!(cfg.repos[0].config().is_none());
        let app = &cfg.repos[1].config().unwrap().services["app"];
        assert!(matches!(app.http, Some(HttpDecl::Enabled(true))));
    }

    #[test]
    fn worktree_base_defaults_to_workspace_dir_name() {
        let r = RemoteConfig { connection: "h".into(), workspace_path: None };
        assert_eq!(r.worktree_base(Path::new("/home/me/myapp")), "myapp");
        let r2 = RemoteConfig { workspace_path: Some("ws/x".into()), ..r };
        assert_eq!(r2.worktree_base(Path::new("/home/me/myapp")), "ws/x");
    }
}
