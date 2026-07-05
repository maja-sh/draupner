use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Deserialize, Serialize, Default, schemars::JsonSchema)]
pub struct WsConfig {
    #[serde(skip_serializing_if = "Option::is_none", default, rename = "$schema")]
    pub schema: Option<String>,
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
    /// Container runtime on the server. Informational for now — hooks invoke it.
    #[serde(default = "default_runtime")]
    pub runtime: String,
    /// Private TLD for per-worktree hostnames (routing; not yet consumed by `ws exec`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
}

impl RemoteConfig {
    /// Construct a remote block with the default runtime.
    pub fn new(connection: String, workspace_path: Option<String>, domain: Option<String>) -> Self {
        Self { connection, workspace_path, runtime: default_runtime(), domain }
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

fn default_runtime() -> String {
    "podman".to_string()
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
    /// Per-event hook path overrides. Keys are event names — see the hook contract
    /// in `spec-2-ops.md`. Values are paths relative to the config file's dir.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub hooks: HashMap<String, String>,
    /// Per-worktree git-exclude entries appended to <wt>/.git/info/exclude.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub worktree_excludes: Vec<String>,
}

impl WsConfig {
    /// Resolve a repo's hook-path override for `event`, if it declares one.
    /// The single lookup shared by the hook-running ops.
    pub fn hook_override(&self, repo: &str, event: &str) -> Option<&str> {
        self.repos
            .iter()
            .find(|r| r.name() == repo)
            .and_then(|r| r.hook_override(event))
    }
}

impl RepoEntry {
    pub fn name(&self) -> &str {
        match self {
            RepoEntry::Name(s) => s,
            RepoEntry::Extended(c) => &c.name,
        }
    }

    pub fn hook_override(&self, event: &str) -> Option<&str> {
        match self {
            RepoEntry::Name(_) => None,
            RepoEntry::Extended(c) => c.hooks.get(event).map(String::as_str),
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
    fn parses_remote_block_and_defaults_runtime() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".ws.json"),
            r#"{"repos":["api"],"remote":{"connection":"srv","workspace_path":"workspace"}}"#,
        )
        .unwrap();
        let remote = load(dir.path()).unwrap().remote.expect("remote block");
        assert_eq!(remote.connection, "srv");
        assert_eq!(remote.workspace_path.as_deref(), Some("workspace"));
        assert_eq!(remote.runtime, "podman");
        assert!(remote.domain.is_none());
    }

    #[test]
    fn worktree_base_defaults_to_workspace_dir_name() {
        let r = RemoteConfig {
            connection: "h".into(),
            workspace_path: None,
            runtime: "podman".into(),
            domain: None,
        };
        assert_eq!(r.worktree_base(Path::new("/home/me/myapp")), "myapp");
        let r2 = RemoteConfig { workspace_path: Some("ws/x".into()), ..r };
        assert_eq!(r2.worktree_base(Path::new("/home/me/myapp")), "ws/x");
    }
}
