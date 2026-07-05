use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Deserialize, Serialize, Default, schemars::JsonSchema)]
pub struct WsConfig {
    #[serde(skip_serializing_if = "Option::is_none", default, rename = "$schema")]
    pub schema: Option<String>,
    #[serde(default)]
    pub repos: Vec<RepoEntry>,
}

/// Per-machine overlay, read from `.ws.json.local` (gitignored). Absent on
/// machines that run everything locally; present with a `remote` block on a
/// machine that offloads its stack to a server. See `spec-remote.md`.
#[derive(Deserialize, Serialize, Default, schemars::JsonSchema)]
pub struct LocalConfig {
    #[serde(skip_serializing_if = "Option::is_none", default, rename = "$schema")]
    pub schema: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub remote: Option<RemoteConfig>,
}

/// v2 remote-offload block. When present, every repo's stack runs on the
/// server and location-transparent ops (`ws exec`, later `up`/`down`) target it.
#[derive(Deserialize, Serialize, schemars::JsonSchema)]
pub struct RemoteConfig {
    /// SSH host to reach the server — a Tailscale MagicDNS name or `~/.ssh/config` alias.
    pub connection: String,
    /// Parent dir on the server holding the synced worktrees. Relative paths are
    /// resolved against the SSH login home; absolute paths are used verbatim.
    pub workspace_path: String,
    /// Container runtime on the server. Informational for now — hooks invoke it.
    #[serde(default = "default_runtime")]
    pub runtime: String,
    /// Private TLD for per-worktree hostnames (routing; not yet consumed by `ws exec`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
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

impl RepoEntry {
    #[allow(dead_code)]
    pub fn name(&self) -> &str {
        match self {
            RepoEntry::Name(s) => s,
            RepoEntry::Extended(c) => &c.name,
        }
    }

    #[allow(dead_code)]
    pub fn hook_override(&self, event: &str) -> Option<&str> {
        match self {
            RepoEntry::Name(_) => None,
            RepoEntry::Extended(c) => c.hooks.get(event).map(String::as_str),
        }
    }

    #[allow(dead_code)]
    pub fn worktree_excludes(&self) -> &[String] {
        match self {
            RepoEntry::Name(_) => &[],
            RepoEntry::Extended(c) => &c.worktree_excludes,
        }
    }
}

/// Load a workspace config from `<workspace-root>/.ws.json`.
#[allow(dead_code)]
pub fn load(root: &Path) -> Result<WsConfig> {
    read_json(&root.join(".ws.json"))
}

/// Load the per-machine overlay from `<workspace-root>/.ws.json.local`. A
/// missing file is not an error — it means "no local overrides" (default).
#[allow(dead_code)]
pub fn load_local(root: &Path) -> Result<LocalConfig> {
    let p = root.join(".ws.json.local");
    if !p.exists() {
        return Ok(LocalConfig::default());
    }
    read_json(&p)
}

#[allow(dead_code)]
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
    fn load_local_missing_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let local = load_local(dir.path()).unwrap();
        assert!(local.remote.is_none());
    }

    #[test]
    fn load_local_parses_remote_block_and_defaults_runtime() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".ws.json.local"),
            r#"{"remote":{"connection":"srv","workspace_path":"workspace"}}"#,
        )
        .unwrap();
        let remote = load_local(dir.path()).unwrap().remote.expect("remote block");
        assert_eq!(remote.connection, "srv");
        assert_eq!(remote.workspace_path, "workspace");
        assert_eq!(remote.runtime, "podman");
        assert!(remote.domain.is_none());
    }
}
