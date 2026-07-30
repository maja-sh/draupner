use anyhow::Result;

/// `ws setup` — ready the *client* for per-worktree routing. Loads the workspace
/// config for the `domain`/`runtime`, resolves where the routing data plane lives,
/// then hands off to the platform-aware setup walk (rootless bits directly;
/// privileged bits shown + consented on macOS, or pointed at the setup script on
/// Linux).
///
/// This runs for a remote-offload workspace too. The stack and its Caddy live on
/// the server, but the client still needs `*.<domain>` resolving to the *server's*
/// address and the *server's* Caddy root CA trusted locally — without both, the
/// browser can neither reach nor trust `https://<branch>.<svc>.<domain>`. Only the
/// rootless local-Caddy spinup is skipped (see `setup::run`).
pub fn run(yes: bool, port: Option<u16>) -> Result<()> {
    let ws = crate::workspace::locate()?;
    let cfg = crate::config::load(&ws.root)?;
    let site = crate::remote::Site::resolve(&cfg)?;
    let address = routing_address(&cfg)?;
    // Where Caddy *binds* at its site — distinct from `address`, which is where
    // this machine dials it. They coincide locally (both loopback) and diverge on
    // a server (bind all interfaces, dial its routable address).
    let configured = cfg.remote.as_ref().and_then(|r| r.publish_address.as_deref());
    let publish = crate::caddy::publish_address(&site, configured);
    crate::setup::run(&site, cfg.domain.as_deref(), cfg.runtime(), yes, port, &address, &publish)
}

/// The address this machine must reach the routing Caddy at.
///
/// Local workspace: loopback. Remote: `remote.publish_address` when it names a
/// concrete address (the documented narrowing — if you pinned Caddy's bind to the
/// server's tailnet/LAN address, that is exactly the address to reach it at),
/// otherwise derived from the SSH connection. A bind-*all* wildcard is not an
/// address you can dial, so it falls through to derivation like an unset value.
fn routing_address(cfg: &crate::config::WsConfig) -> Result<String> {
    let Some(remote) = &cfg.remote else {
        return Ok("127.0.0.1".to_string());
    };
    let pinned = remote
        .publish_address
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .filter(|a| !is_wildcard(a));
    if let Some(addr) = pinned {
        return Ok(addr.to_string());
    }
    crate::remote::routing_address(&remote.connection).ok_or_else(|| {
        crate::ops::op_failure(format!(
            "couldn't work out an address for the server `{}` — `ssh -G` didn't report a \
             resolvable hostname. Set `remote.publish_address` in .ws.json to the address \
             this machine reaches the server at.",
            remote.connection
        ))
    })
}

/// Whether an address is a bind-any wildcard (`0.0.0.0` / `::`) rather than a
/// dialable address. `config::load` already rejected a non-IP `publish_address`,
/// so parsing here only ever sorts valid addresses.
fn is_wildcard(addr: &str) -> bool {
    addr.parse::<std::net::IpAddr>().map(|ip| ip.is_unspecified()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RemoteConfig, WsConfig};

    // `.invalid` is reserved by RFC 2606 and can never resolve, so derivation
    // deterministically fails — the tests here are about *precedence*, not DNS.
    fn remote_cfg(publish: Option<&str>) -> WsConfig {
        let mut r = RemoteConfig::new("ws-no-such-server.invalid".to_string(), None);
        r.publish_address = publish.map(str::to_string);
        WsConfig { remote: Some(r), ..WsConfig::default() }
    }

    #[test]
    fn local_workspace_targets_loopback() {
        let cfg = WsConfig::default();
        assert_eq!(routing_address(&cfg).unwrap(), "127.0.0.1");
    }

    #[test]
    fn pinned_publish_address_is_the_target() {
        let cfg = remote_cfg(Some("100.64.1.2"));
        assert_eq!(routing_address(&cfg).unwrap(), "100.64.1.2");
    }

    #[test]
    fn bind_any_wildcards_are_not_treated_as_a_target() {
        // `0.0.0.0` is the *default bind*, not somewhere a browser can connect —
        // taking it literally would wire the client's DNS to a dead address. It
        // must fall through to SSH derivation (which fails here: no such host).
        for wildcard in ["0.0.0.0", "::", " 0.0.0.0 "] {
            let cfg = remote_cfg(Some(wildcard));
            assert!(
                routing_address(&cfg).is_err(),
                "`{wildcard}` must not be used as a reachable address"
            );
        }
    }

    #[test]
    fn unresolvable_connection_explains_the_publish_address_override() {
        let cfg = remote_cfg(None);
        let err = routing_address(&cfg).unwrap_err().to_string();
        assert!(err.contains("remote.publish_address"), "{err}");
    }
}
