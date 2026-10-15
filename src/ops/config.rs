//! `drup config` get/set/unset with git-style surface: bare value on stdout, exit 1 when unset.

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
            // Git parity: unset key is exit 1 with nothing on stdout.
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
    // Skip the write on a no-op unset so we never create the file just to say so.
    if cfg.unset(key).map_err(|e| super::preflight(e.to_string()))? {
        cfg.save()?;
    }
    Ok(())
}
