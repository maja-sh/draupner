//! `ws config` — get/set the machine-global defaults in
//! `~/.config/ws/config.toml` (see `config::GlobalConfig`). Git-style surface:
//! `get` prints the bare value on stdout (scriptable) and exits 1 when unset;
//! `set`/`unset` mutate the file and stay quiet on success.

use anyhow::Result;

use crate::config::GlobalConfig;

pub fn get(key: &str) -> Result<()> {
    let cfg = GlobalConfig::load()?;
    match cfg.get(key).map_err(|e| super::preflight(e.to_string()))? {
        Some(v) => {
            crate::ui::value(v);
            Ok(())
        }
        None => {
            // Git parity: an unset key is exit 1 with nothing on stdout. The
            // stderr note keeps the human case self-explanatory; `silent` skips
            // the failure banner (the note already said everything).
            crate::ui::note(&format!("{key} is not set"));
            Err(super::passthrough_exit(1))
        }
    }
}

pub fn set(key: &str, value: &str) -> Result<()> {
    let mut cfg = GlobalConfig::load()?;
    cfg.set(key, value).map_err(|e| super::preflight(e.to_string()))?;
    cfg.save()
}

pub fn unset(key: &str) -> Result<()> {
    let mut cfg = GlobalConfig::load()?;
    // Unsetting an already-unset key is a no-op, not an error (idempotent for
    // scripts) — but skip the write so we never create the file just to say so.
    if cfg.unset(key).map_err(|e| super::preflight(e.to_string()))? {
        cfg.save()?;
    }
    Ok(())
}
