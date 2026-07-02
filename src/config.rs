use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Deserialize, Serialize, Default, schemars::JsonSchema)]
pub struct WsConfig {
    #[serde(skip_serializing_if = "Option::is_none", default, rename = "$schema")]
    pub schema: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub remote: Option<RemoteConfig>,
    #[serde(default)]
    pub repos: Vec<RepoEntry>,
}

/// v2 remote-offload block. Present as a stub in v1 so the schema is stable
/// once we start writing `.ws.json` files — no logic reads it yet.
#[derive(Deserialize, Serialize, schemars::JsonSchema)]
pub struct RemoteConfig {
    pub host: String,
    #[serde(default)]
    pub remote_root: Option<String>,
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

#[allow(dead_code)]
fn read_json<T: for<'de> Deserialize<'de>>(p: &Path) -> Result<T> {
    let raw = std::fs::read_to_string(p)
        .with_context(|| format!("read {}", p.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parse {}", p.display()))
}
