use anyhow::Result;

/// `ws setup` — ready the host for per-worktree routing. Loads the workspace
/// config for the `domain`/`runtime`, then hands off to the platform-aware
/// setup walk (rootless bits directly; privileged bits shown + consented on
/// macOS, or pointed at the setup script on Linux).
pub fn run(yes: bool, port: Option<u16>) -> Result<()> {
    let ws = crate::workspace::locate()?;
    let cfg = crate::config::load(&ws.root)?;
    crate::setup::run(cfg.domain.as_deref(), cfg.runtime(), yes, port)
}
