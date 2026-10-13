use anyhow::Result;

/// `wsx setup` — ready the *client* for per-worktree routing. Runs even for
/// remote-offload workspaces: the browser still needs `*.<domain>` resolving to
/// the server and the server's Caddy root CA trusted locally.
pub fn run(yes: bool, port: Option<u16>, admin_port: Option<u16>) -> Result<()> {
    let ws = crate::workspace::locate()?;
    let cfg = crate::config::load(&ws.root)?;
    let site = crate::remote::Site::resolve(&cfg)?;
    let address = routing_address(&cfg)?;

    // Persist the port opt-ins before `setup::run` reads them back.
    if let Some(p) = port {
        crate::caddy::set_https_port(p)?;
        crate::ui::note(&format!("HTTPS port set to {p} (persisted)"));
    }
    if let Some(p) = admin_port {
        crate::caddy::set_admin_port(p)?;
        crate::ui::note(&format!("Caddy admin port set to {p} (persisted)"));
    }

    // `publish` is where Caddy *binds*; `address` is where this machine dials
    // it — they diverge on a server.
    let configured = cfg.remote.as_ref().and_then(|r| r.publish_address.as_deref());
    let publish = crate::caddy::publish_address(&site, configured);
    crate::setup::run(&site, cfg.domain.as_deref(), cfg.runtime(), yes, &address, &publish)
}

/// The address this machine must reach the routing Caddy at — a pinned
/// non-wildcard `remote.publish_address` if set, else derived from SSH.
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
             resolvable hostname. Set `remote.publish_address` in .wsx.json to the address \
             this machine reaches the server at.",
            remote.connection
        ))
    })
}

/// Whether an address is a bind-any wildcard (`0.0.0.0` / `::`).
fn is_wildcard(addr: &str) -> bool {
    addr.parse::<std::net::IpAddr>().map(|ip| ip.is_unspecified()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RemoteConfig, WsConfig};

    // `.invalid` (RFC 2606) never resolves, so SSH derivation fails deterministically.
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
        // `0.0.0.0` is a *bind* default, not a dialable address.
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
