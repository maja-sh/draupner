//! `wsx share` / `wsx unshare` — make a local task's routed services reachable from
//! other machines on the network, one task at a time.

use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::config;
use crate::inject;
use crate::remote::Site;
use crate::workspace;

/// Loopback is what the private listener already serves; the wildcard would collide with it on the same port.
const LOCAL_PUBLISH: &str = "127.0.0.1";

fn marker(root: &Path, sanitized: &str) -> PathBuf {
    crate::ports::meta_dir(root).join("shared").join(sanitized)
}

pub fn is_shared(root: &Path, sanitized: &str) -> bool {
    marker(root, sanitized).is_file()
}

fn mark(root: &Path, sanitized: &str) -> Result<()> {
    let path = marker(root, sanitized);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, b"")?;
    Ok(())
}

pub fn unmark(root: &Path, sanitized: &str) {
    let _ = std::fs::remove_file(marker(root, sanitized));
}

/// Unpublishes the share listener once no task on this machine still has a share route.
pub fn retire_if_idle(site: &Site, runtime: &str) {
    if site.is_remote() || crate::caddy::externally_managed() || !crate::caddy::is_running(site, runtime) {
        return;
    }
    if crate::caddy::shared_route_count(site) == Some(0) {
        crate::caddy::drop_share();
        if let Err(e) = crate::caddy::ensure_running(site, runtime, LOCAL_PUBLISH) {
            crate::ui::note(&format!("could not unpublish the share listener: {e:#}"));
        }
    }
}

fn validate_address(a: &str) -> std::result::Result<(), String> {
    match a.parse::<std::net::IpAddr>() {
        Ok(ip) if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() => Err(format!(
            "`{a}` can't be a share address — give this machine's address on the network the other \
             machines are on"
        )),
        Ok(_) => Ok(()),
        Err(_) => Err(format!("`{a}` is not an IP address")),
    }
}

pub fn share(branch: &str, address: Option<&str>) -> Result<()> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;
    let sanitized = workspace::validate_branch(branch).map_err(super::preflight)?;
    let Some(domain) = cfg.domain.as_deref() else {
        return Err(super::preflight(
            "sharing goes through routing — set `domain` in .wsx.json (see docs/sharing.md)",
        ));
    };
    if cfg.remote.is_some() {
        return Err(super::preflight(
            "an offloaded workspace is already reachable on the server's networks \
             (narrow it with `remote.publish_address`); `wsx share` is for local workspaces",
        ));
    }
    if let Some(a) = address {
        validate_address(a).map_err(super::preflight)?;
    }
    let targets = workspace::task_worktrees(&ws.root, &cfg, branch);
    if targets.is_empty() {
        return Err(super::preflight(format!("no worktrees for {branch}; run `wsx new` first")));
    }

    crate::ui::header("wsx-share", branch);
    let site = Site::Local;

    let suffix = crate::caddy::port_suffix(crate::caddy::https_port());
    let lan = if crate::caddy::externally_managed() {
        crate::ui::note("Caddy is externally managed (WSX_CADDY_ADMIN) — publishing its share listener is up to you");
        None
    } else {
        crate::caddy::want_share(address)?;
        let step = crate::ui::step("caddy", "share listener");
        let Some(addr) = crate::caddy::share_address(&site) else {
            step.fail("couldn't find this machine's network address");
            return Err(super::op_failure(format!("pass it explicitly: `wsx share {branch} --address <ip>`")));
        };
        let res = crate::caddy::ensure_running(&site, cfg.runtime(), LOCAL_PUBLISH)
            .and_then(|()| crate::caddy::ensure_share_server(&site));
        if let Err(e) = res {
            step.fail(&format!("{e:#}"));
            return Err(super::op_failure("the share listener isn't up"));
        }
        step.ok_with(&format!("{addr}{suffix}"));
        Some(addr)
    };
    mark(&ws.root, &sanitized)?;

    let cert = sync_cert(&site, domain);

    let mut hosts = Vec::new();
    let mut pending = Vec::new();
    for (repo, _) in &targets {
        let routes = inject::routes(&cfg, &ws.root, &sanitized, repo);
        if routes.is_empty() {
            continue;
        }
        if !routes.iter().any(|r| crate::caddy::route_live(&site, &r.id)) {
            crate::ui::skip(repo, "not up — shared on the next `wsx up`");
            pending.push(repo.clone());
            continue;
        }
        let step = crate::ui::step(repo, "shared");
        let mut urls = Vec::new();
        let mut err = None;
        for r in &routes {
            match crate::caddy::share_route_up(&site, r) {
                Ok(()) => urls.push(format!("https://{}{suffix}", r.hostname)),
                Err(e) => err = Some(format!("{e:#}")),
            }
            hosts.push(r.hostname.clone());
        }
        match err {
            None => step.ok_with(&urls.join(", ")),
            Some(e) => step.fail(&e),
        }
    }

    report_coverage(cert.as_deref(), domain, &hosts);
    if let Some(addr) = &lan {
        crate::ui::note(&format!(
            "the other machines need `*.{domain}` to resolve to {addr} — a wildcard record in \
             DNS they use, or one hosts line per URL"
        ));
    }
    let mut lines = vec![format!("Shared {branch}")];
    if !pending.is_empty() {
        lines.push(format!("Waiting for `wsx up`: {}", pending.join(", ")));
    }
    crate::ui::summary_ok(&lines);
    crate::ui::nudge(&format!("wsx unshare {branch} to stop"));
    Ok(())
}

fn sync_cert(site: &Site, domain: &str) -> Option<PathBuf> {
    let step = crate::ui::step("cert", "certificate");
    match crate::certs::sync(site, domain) {
        Ok(Some(dir)) => {
            step.ok_with(&dir.display().to_string());
            crate::certs::cert_path(domain).ok()
        }
        Ok(None) => {
            let dir = crate::certs::dir(domain).map(|d| d.display().to_string()).unwrap_or_default();
            step.ok_with("none — Caddy's internal CA");
            crate::ui::note(&format!(
                "other machines won't trust Caddy's internal CA; put a publicly trusted \
                 fullchain.pem + privkey.pem in {dir} (see docs/sharing.md)"
            ));
            None
        }
        Err(e) => {
            step.fail(&format!("{e:#}"));
            None
        }
    }
}

fn report_coverage(cert: Option<&Path>, domain: &str, hosts: &[String]) {
    let Some(cert) = cert else {
        if !hosts.is_empty() {
            crate::ui::note(&format!(
                "a certificate for these needs: {}",
                crate::certs::wanted_names(hosts).join(" ")
            ));
        }
        return;
    };
    if crate::certs::expires_within(cert, 0) == Some(true) {
        crate::ui::warn(&format!("the certificate for {domain} has expired"), &[]);
    } else if crate::certs::expires_within(cert, 7 * 24 * 3600) == Some(true) {
        crate::ui::note(&format!("the certificate for {domain} expires within a week — renew it"));
    }
    let Some(names) = crate::certs::names(cert) else { return };
    let uncovered: Vec<String> =
        hosts.iter().filter(|h| !crate::certs::covers(&names, h)).cloned().collect();
    if !uncovered.is_empty() {
        crate::ui::warn(
            "the certificate doesn't cover these — other machines will get Caddy's internal CA",
            &uncovered,
        );
        crate::ui::note(&format!("reissue it with: {}", crate::certs::wanted_names(&uncovered).join(" ")));
    }
}

pub fn unshare(branch: &str) -> Result<()> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;
    let sanitized = workspace::validate_branch(branch).map_err(super::preflight)?;
    if !is_shared(&ws.root, &sanitized) {
        return Err(super::preflight(format!("{branch} isn't shared")));
    }

    crate::ui::header("ws-unshare", branch);
    let site = Site::Local;
    unmark(&ws.root, &sanitized);
    for (repo, _) in workspace::task_worktrees(&ws.root, &cfg, branch) {
        for r in inject::routes(&cfg, &ws.root, &sanitized, &repo) {
            crate::caddy::share_route_down(&site, &r.id);
        }
    }
    retire_if_idle(&site, cfg.runtime());
    crate::ui::summary_ok(&[format!("Stopped sharing {branch}")]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_share_address_must_be_a_concrete_reachable_ip() {
        assert!(validate_address("192.168.1.20").is_ok());
        assert!(validate_address("fd7a::1").is_ok());
        for bad in ["0.0.0.0", "::", "127.0.0.1", "::1", "224.0.0.1", "my-laptop.local"] {
            assert!(validate_address(bad).is_err(), "{bad} must be refused");
        }
    }

    #[test]
    fn the_marker_tracks_share_state_per_task() {
        let root = tempfile::tempdir().unwrap();
        assert!(!is_shared(root.path(), "feat-x"));
        mark(root.path(), "feat-x").unwrap();
        assert!(is_shared(root.path(), "feat-x"));
        assert!(!is_shared(root.path(), "feat-y"));
        unmark(root.path(), "feat-x");
        assert!(!is_shared(root.path(), "feat-x"));
    }
}
