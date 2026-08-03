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
            // Git parity: an unset key is exit 1 with nothing on stdout, so the
            // note goes to stderr and carries the failure on its own.
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
    // Unsetting an unset key is a no-op rather than an error, and skips the
    // write so it never creates the file just to say so.
    if cfg.unset(key).map_err(|e| super::preflight(e.to_string()))? {
        cfg.save()?;
    }
    Ok(())
}
