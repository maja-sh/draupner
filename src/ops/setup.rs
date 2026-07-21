use anyhow::Result;

/// `ws setup` — ready the host for per-worktree routing. Loads the workspace
/// config for the `domain`/`runtime`, then hands off to the platform-aware
/// setup walk (rootless bits directly; privileged bits shown + consented on
/// macOS, or pointed at the setup script on Linux).
pub fn run(yes: bool, port: Option<u16>) -> Result<()> {
    let ws = crate::workspace::locate()?;
    let cfg = crate::config::load(&ws.root)?;
    // `ws setup` readies the *local host* (its Caddy, DNS, CA). For a remote-offload
    // workspace the stack — and its Caddy — live on the server, so setting up the
    // local host would spin up a pointless local Caddy (and could fight for :443).
    // Point the user at the server-side path instead of doing the wrong thing.
    if cfg.remote.is_some() {
        crate::ui::note(
            "this workspace offloads to a remote server, so there's nothing to set up on the \
             local host. Provision the server with `server-setup/<distro>.sh` and verify with \
             `ws remote check`.",
        );
        return Ok(());
    }
    // `ws setup` only ever runs against the local host (remote returns above).
    let publish = crate::caddy::publish_address(&crate::remote::Site::Local, None);
    crate::setup::run(cfg.domain.as_deref(), cfg.runtime(), yes, port, &publish)
}
