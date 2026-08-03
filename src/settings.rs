use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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

const GLOBAL_KEYS: &[&str] = &["remote.host"];

fn unknown_key(key: &str) -> anyhow::Error {
    anyhow::anyhow!("unknown config key `{key}` — known keys: {}", GLOBAL_KEYS.join(", "))
}

impl GlobalConfig {
    fn path() -> PathBuf {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
