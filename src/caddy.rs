//! Containerized Caddy on the shared `ws-net`, driven by its admin API.
//! See ARCHITECTURE.md for the topology and the route-CRUD contract.
//!
//! Two constraints that explain shapes here which otherwise look wrong:
//! Caddy runs as a *container* rather than on the host because a rootless host
//! Caddy can't resolve container DNS (aardvark/netavark's resolver lives inside
//! the container netns). And the admin API is reached over a loopback-published
//! TCP port rather than a unix socket because a socket can't cross the
//! podman-machine VM boundary on macOS.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

use crate::remote::Site;

pub const WS_NET: &str = "ws-net";
pub const CADDY_CONTAINER: &str = "ws-caddy";
const CADDY_IMAGE: &str = "docker.io/library/caddy:2";
/// The HTTP server block name inside Caddy's config; routes hang off it.
const SERVER: &str = "ws";
/// Non-privileged, so the rootless path needs zero setup. `:443` only buys
/// suffix-free browser URLs and costs the `net.ipv4.ip_unprivileged_port_start`
/// sysctl, so it's an opt-in (`ws setup --port 443`).
pub const DEFAULT_HTTPS_PORT: u16 = 8443;
/// Mount point inside the Caddy container for the bind-mounted machine-global
/// config dir ([`crate::settings::global_dir`]), holding the pinned boot config
/// (`caddy.json`).
const SOCK_MOUNT: &str = "/sock";
/// 2019 is *Caddy's* default, not a port `ws` picked — so a box already running
/// its own Caddy holds it, and the collision is likely rather than unlucky.
const DEFAULT_ADMIN_PORT: u16 = 2019;
/// `/data`: the internal CA root + issued leaf certs. Persisted so the root stays
/// stable across container recreation and a client only has to trust it once.
const DATA_VOLUME: &str = "ws-caddy-data";
/// `/config`: `caddy run --resume`'s autosave. Persisted so a recreated `ws-caddy`
/// reloads every worktree route — they otherwise live only in Caddy's memory.
const CONFIG_VOLUME: &str = "ws-caddy-config";
/// Distinct from any `route_id`, so worktree route CRUD never touches the
/// catch-all.
const STOPPED_STACK_ID: &str = "ws-stopped-stack";
/// How long the admin API gets to answer before `ws` treats silence as final.
/// One budget rather than one per caller, because the two callers are asking the
/// same question — [`wait_admin_ready`] of a Caddy it just started, and
/// [`served_port`] of one a peer may be starting right now.
const ADMIN_POLLS: u32 = 50;
const ADMIN_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// No `Site` is needed even for a remote site: the admin is published on the
/// *server's* loopback and `curl` itself runs there (see [`curl`]), so
/// `127.0.0.1` resolves server-side.
fn admin_url(path: &str) -> String {
    if let Ok(url) = std::env::var("WS_CADDY_ADMIN") {
        format!("{}{}", url.trim_end_matches('/'), path)
    } else {
        format!("http://127.0.0.1:{}{path}", admin_port())
    }
}

/// Whether Caddy + `ws-net` are managed outside `ws`. In that mode `ws` still
/// adds/removes routes against the admin API but creates no network and runs no
/// container.
///
/// `WS_CADDY_MANAGE` decides explicitly (`external` / `managed`); without it, a
/// set `WS_CADDY_ADMIN` implies external, because pointing `ws` at an admin it
/// didn't boot has always meant "bring your own Caddy". The explicit `managed`
/// exists so a test can redirect the admin *and* still exercise the container
/// lifecycle — under the implied rule, everything behind this check was
/// untestable through the CLI.
pub fn externally_managed() -> bool {
    match std::env::var("WS_CADDY_MANAGE").as_deref() {
        Ok("external") => true,
        Ok("managed") => false,
        _ => std::env::var_os("WS_CADDY_ADMIN").is_some(),
    }
}

/// A machine-wide port `ws` persists: the `env` seam wins, else
/// `~/.config/ws/<file>`, else `default`. A `0` is rejected *at each layer* so a
/// truncated file or a bad env value falls through to the next one instead of
/// seeding a Caddy that listens on port 0.
fn persisted_port(env: &str, file: &str, default: u16) -> u16 {
    let nonzero = |p: u16| (p != 0).then_some(p);
    std::env::var(env)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .and_then(nonzero)
        .or_else(|| {
            std::fs::read_to_string(crate::settings::global_dir().join(file))
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .and_then(nonzero)
        })
        .unwrap_or(default)
}

/// The HTTPS port Caddy publishes and serves on. A change only takes effect once
/// `ensure_running` re-seeds — `--resume` otherwise pins the old port from the
/// autosave.
pub fn https_port() -> u16 {
    persisted_port("WS_HTTPS_PORT", "https_port", DEFAULT_HTTPS_PORT)
}

/// The `:port` suffix a browser URL carries, empty for the standard `:443`.
pub fn port_suffix(port: u16) -> String {
    if port == 443 {
        String::new()
    } else {
        format!(":{port}")
    }
}

/// The address Caddy publishes its HTTPS port on. A local site ignores
/// `configured` deliberately: `resolve_hint`, `resolve_domain` and `ws setup`'s
/// CA probe all assume a local Caddy is on loopback.
pub fn publish_address(site: &Site, configured: Option<&str>) -> String {
    match site {
        Site::Local => "127.0.0.1".to_string(),
        Site::Remote { .. } => configured
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .unwrap_or("0.0.0.0")
            .to_string(),
    }
}

/// IPv6 hosts must be bracketed or the runtime can't tell the address from the
/// port and rejects the whole `-p` — stranding exactly the user narrowing their
/// exposure.
fn publish_spec(addr: &str, port: u16) -> String {
    match addr.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(a)) => format!("[{a}]:{port}:{port}"),
        _ => format!("{addr}:{port}:{port}"),
    }
}

/// The host address the running Caddy actually publishes on. `None` means "can't
/// tell" — callers must never read it as a mismatch.
fn published_host(site: &Site, runtime: &str, port: u16) -> Option<String> {
    let out = site
        .command(&[runtime, "port", CADDY_CONTAINER, &port.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout).lines().next()?.trim().to_string();
    // Split at the *last* colon so an IPv6 host (`[::1]:8443`) survives intact.
    let host = line.rsplit_once(':')?.0.to_string();
    (!host.is_empty()).then(|| host.trim_matches(['[', ']']).to_string())
}

/// Write one of the persisted ports [`persisted_port`] reads back. The trailing
/// newline keeps the file hand-editable.
fn set_persisted_port(file: &str, port: u16) -> Result<()> {
    let dir = crate::settings::global_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::write(dir.join(file), format!("{port}\n"))
        .with_context(|| format!("persist {file}"))
}

pub fn set_https_port(port: u16) -> Result<()> {
    set_persisted_port("https_port", port)
}

/// The admin API port `ws` publishes to the site's loopback. Every route CRUD
/// call dials it, so it must agree across runs — hence persisted, never picked
/// fresh per invocation.
pub fn admin_port() -> u16 {
    persisted_port("WS_CADDY_ADMIN_PORT", "admin_port", DEFAULT_ADMIN_PORT)
}

pub fn set_admin_port(port: u16) -> Result<()> {
    set_persisted_port("admin_port", port)
}

/// A route's stable admin `@id`. The repo is part of it so two repos in one
/// branch exposing a same-named service (both `web`) don't collide on one `@id`
/// and delete each other's route.
pub fn route_id(branch_sanitized: &str, repo: &str, service: &str) -> String {
    format!("ws-{}-{}-{}", id_part(branch_sanitized), id_part(repo), id_part(service))
}

/// Collapse one `@id` component to what is safe in a single URL path segment.
///
/// The id is interpolated into the admin path as `/id/<id>/`, and `.ws.json`
/// isn't gitignored — so a cloned repo naming a service `/../../config` would
/// turn one route delete into `DELETE /config/` (curl removes dot segments
/// before sending), wiping the running Caddy config and every other workspace's
/// live routes. `-` is the replacement because `sanitize_branch` and
/// `inject::dns_label` use it, so a dotted name keeps agreeing with the hostname
/// it's routed under.
fn id_part(s: &str) -> String {
    s.replace(|c: char| !(c.is_ascii_alphanumeric() || c == '_'), "-")
}

pub struct RouteSpec {
    pub id: String,
    pub hostname: String,
    /// `<project>-<svc>-1:<port>` for a container, `host.containers.internal:<port>`
    /// for a host process.
    pub upstream: String,
    /// Also match `*.<hostname>` — every tenant subdomain.
    pub wildcard: bool,
}

pub fn ensure_net(site: &Site, runtime: &str) -> Result<()> {
    if externally_managed() {
        return Ok(());
    }
    let out = site
        .command(&[runtime, "network", "create", WS_NET])
        .output()
        .with_context(|| format!("spawn `{runtime} network create`"))?;
    if out.status.success() {
        return Ok(());
    }
    // An already-existing network is the common, benign case (concurrent `ws up`s
    // race to create it). Matched loosely so both the podman and docker phrasings
    // land.
    let err = String::from_utf8_lossy(&out.stderr);
    let lower = err.to_lowercase();
    if lower.contains("already") || lower.contains("exists") {
        return Ok(());
    }
    bail!("create {WS_NET}: {}", err.trim());
}

/// Ensure the singleton Caddy container is running, booting it from a pinned
/// config if not.
///
/// One `ws-caddy` serves every workspace on the machine, so removing a running
/// one takes a peer's live routes down with it. Two arms below still do that —
/// a wedged admin and a port move — and each says why it is worth the outage;
/// an address disagreement defers to [`reconcile_publish`], which consults the
/// live routes before anything is allowed to be destructive. Everything else —
/// a name clash from a peer that already has it up, a stale *stopped*
/// container — is handled without touching a live one.
pub fn ensure_running(site: &Site, runtime: &str, publish_addr: &str) -> Result<()> {
    if externally_managed() {
        return Ok(());
    }
    let wanted = https_port();
    if container_running(site, runtime) {
        // A running Caddy is left alone unless its live port or publish address
        // disagrees with what's wanted. The comparison is against what Caddy is
        // *actually* serving rather than a recorded marker, so a routine `ws up`
        // never tears a peer's routes down.
        match served_port(site) {
            // The publish address is invisible to Caddy (a `-p` binding is the
            // runtime's business), so `served_port` can never reveal a change to
            // it. Without this arm, pinning `remote.publish_address` to close an
            // exposure would silently keep the old binding, and `--restart always`
            // would carry it across reboots.
            Some(p) if p == wanted => {
                match published_host(site, runtime, p) {
                    Some(h) if h != publish_addr => {
                        if !reconcile_publish(site, runtime, &h, publish_addr)? {
                            return Ok(());
                        }
                    }
                    _ => return Ok(()),
                }
            }
            // Admin still silent after the full boot budget (see `served_port`):
            // it predates the switch to a TCP admin (its unix-socket admin isn't
            // on :2019) or is wedged. `--resume` would restore that old admin
            // from the autosave, so the config volume has to go with it — which
            // costs every workspace on the machine its routes, hence the wait.
            None => {
                crate::ui::note(&format!(
                    "{CADDY_CONTAINER} is running but its admin is unreachable — \
                     re-seeding it (routes reinstall on `ws up`)"
                ));
                reseed(site, runtime);
            }
            // Only move ports if the new one will actually take a listener — else we'd
            // tear a working Caddy down for one that fails to start and leave the
            // machine with no routing at all. Two ways it won't: the sysctl forbids it
            // (`:443` before `ws setup`), or something else already holds it.
            // `preflight_ports` asks the second question too, but only below, after the
            // teardown. `ws setup` reports either gap.
            Some(_)
                if !port_bindable(site, wanted) || !crate::ports::port_free_at(site, wanted) =>
            {
                return Ok(())
            }
            Some(_) => {
                crate::ui::note(&format!(
                    "re-seeding {CADDY_CONTAINER} for the :{wanted} move — routes reinstall on `ws up`"
                ));
                reseed(site, runtime);
            }
        }
    }
    ensure_net(site, runtime)?;

    // A busy port otherwise surfaces as a raw `rootlessport ... address already in
    // use` plus a half-created container. A running `ws-caddy` (ours or a peer's)
    // returned above, so this only fires against a *foreign* holder — typically a
    // box running its own Caddy on :2019.
    preflight_ports(site)?;

    let dir = write_boot_config(site)?;
    if run_caddy(site, runtime, &dir, publish_addr).is_none() {
        return wait_admin_ready(site);
    }
    // The run failed: either a peer already started it (name taken, now running →
    // done) or a stale *stopped* container holds the name.
    if container_running(site, runtime) {
        return wait_admin_ready(site);
    }
    let _ = site.command(&[runtime, "rm", "-f", CADDY_CONTAINER]).output();
    let reason = run_caddy(site, runtime, &dir, publish_addr);
    if reason.is_none() || container_running(site, runtime) {
        return wait_admin_ready(site);
    }
    // Surface the real reason: the container never started, so
    // `podman logs ws-caddy` would be empty.
    bail!(
        "could not start the {CADDY_CONTAINER} container: {}",
        reason.unwrap_or_else(|| "unknown error".to_string())
    )
}

/// Decide what a publish-address disagreement costs, and who pays. Returns
/// whether the container was torn down (so the caller reboots it); `Ok(false)`
/// means the running Caddy stays.
///
/// One `ws-caddy` is shared by every workspace on the machine, and each brings
/// its own wanted address — so "recreate to what I want" was a ping-pong that
/// took the other workspaces' routes down on every `ws up`. The live route list
/// is the arbiter: it is exactly the set of worktrees currently depending on
/// this Caddy, it lives on the right machine by construction, and it cannot go
/// stale the way a registry file would.
fn reconcile_publish(site: &Site, runtime: &str, live: &str, wanted: &str) -> Result<bool> {
    // A wildcard publish serves loopback too, so a local workspace (which
    // always wants loopback) needs nothing from a Caddy an offloaded workspace
    // opened up — and narrowing it would break that workspace for nothing.
    if is_loopback(wanted) && is_unspecified(live) {
        return Ok(false);
    }
    let hostnames = live_route_hostnames(site);
    match hostnames {
        Ok(hosts) if hosts.is_empty() => {
            crate::ui::note(&format!(
                "re-creating {CADDY_CONTAINER} to publish on {wanted} (currently {live}, \
                 no live routes) — routes are preserved"
            ));
            // Not `reseed`: replacing only the container lets `--resume` reload
            // routes from the retained config volume. A port move needs that
            // volume wiped; an address move doesn't.
            let _ = site.command(&[runtime, "rm", "-f", CADDY_CONTAINER]).output();
            Ok(true)
        }
        // The live publish covers what we need, so nothing is broken for us —
        // but the pin this workspace asked for is not applied while others are
        // live, and silently staying wide would defeat its point. Say so.
        Ok(hosts) if covers(live, wanted) => {
            crate::ui::warn(
                &format!(
                    "publish address pin ({wanted}) not applied — {CADDY_CONTAINER} is \
                     serving live routes on {live}"
                ),
                &[
                    format!("live: {}", hosts.join(", ")),
                    "the pin applies on the next `ws up` after those are `ws down`ed".to_string(),
                ],
            );
            Ok(false)
        }
        Ok(hosts) => bail!(
            "{CADDY_CONTAINER} publishes on {live}, but this workspace needs {wanted}, and \
             re-creating it would drop live routes: {}. `ws down` those worktrees first, or \
             align `remote.publish_address` across the workspaces sharing this machine.",
            hosts.join(", ")
        ),
        // Can't tell what's live — never destroy on an unknown.
        Err(e) => bail!(
            "{CADDY_CONTAINER} publishes on {live}, but this workspace needs {wanted}, and \
             its live routes couldn't be read ({e}) — refusing to re-create it blind"
        ),
    }
}

fn is_loopback(addr: &str) -> bool {
    addr.parse::<std::net::IpAddr>().map(|a| a.is_loopback()).unwrap_or(false)
}

fn is_unspecified(addr: &str) -> bool {
    addr.parse::<std::net::IpAddr>().map(|a| a.is_unspecified()).unwrap_or(false)
}

/// Whether a socket published on `live` answers dials aimed at `wanted`.
fn covers(live: &str, wanted: &str) -> bool {
    live == wanted || is_unspecified(live)
}

/// Every hostname the running Caddy routes, minus the stopped-stack catch-all:
/// the set of worktrees that would go dark if this Caddy were removed. Route ids
/// carry no workspace name, so the list says *what* is live, never *whose*.
fn live_route_hostnames(site: &Site) -> Result<Vec<String>> {
    let out = curl(site, &[], &format!("/config/apps/http/servers/{SERVER}/routes"))?;
    if !out.succeeded() {
        bail!("admin API answered HTTP {}", out.code);
    }
    let routes: serde_json::Value = serde_json::from_str(&out.body).context("parse route list")?;
    let mut hosts = Vec::new();
    for route in routes.as_array().map(|a| a.as_slice()).unwrap_or_default() {
        if route.get("@id").and_then(|i| i.as_str()) == Some(STOPPED_STACK_ID) {
            continue;
        }
        for m in route.get("match").and_then(|m| m.as_array()).map(|a| a.as_slice()).unwrap_or_default() {
            for h in m.get("host").and_then(|h| h.as_array()).map(|a| a.as_slice()).unwrap_or_default() {
                if let Some(h) = h.as_str() {
                    hosts.push(h.to_string());
                }
            }
        }
    }
    Ok(hosts)
}

/// Refuse a Caddy run when either port it must publish is already held at `site`.
/// An undetermined answer counts as free — `port_free_at` reports busy only on a
/// positive signal.
fn preflight_ports(site: &Site) -> Result<()> {
    let https = https_port();
    let admin = admin_port();
    for (port, what, flag) in [
        (https, "HTTPS", "--port"),
        (admin, "admin API", "--admin-port"),
    ] {
        if !crate::ports::port_free_at(site, port) {
            let whose = if site.is_remote() { "the server" } else { "this machine" };
            bail!(
                "{what} port :{port} is already in use on {whose} — another service (often an \
                 existing Caddy) holds it. Move ws's Caddy off it with `ws setup {flag} <port>`."
            );
        }
    }
    Ok(())
}

/// The HTTPS port the running Caddy is actually listening on. `None` when the
/// admin never answers, or answers with an unexpected shape.
///
/// Waits out the same budget a boot gets, because `None` is what sends the
/// caller into a re-seed and a peer three seconds into its own `ws up` refuses
/// the connection *instantly* — indistinguishable from a wedged admin without
/// waiting for it. Answering at all settles the question, so a healthy Caddy
/// still costs one round trip.
fn served_port(site: &Site) -> Option<u16> {
    for _ in 0..ADMIN_POLLS {
        let listen = curl(site, &[], &format!("/config/apps/http/servers/{SERVER}/listen"));
        if let Ok(out) = listen {
            if out.succeeded() {
                return parse_listen_port(&out.body);
            }
        }
        std::thread::sleep(ADMIN_POLL_INTERVAL);
    }
    None
}

/// Pull the port out of Caddy's `listen` array JSON (`[":8443"]` /
/// `["127.0.0.1:8443"]`).
fn parse_listen_port(body: &str) -> Option<u16> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get(0)?.as_str()?.rsplit(':').next()?.parse().ok()
}

/// Tear down a Caddy so a fresh boot config can seed it. The config volume must
/// go too: it holds the autosave `--resume` would reload, which is what pins the
/// old port. Routes live there as well, so they reinstall on the next `ws up`.
/// `ws-caddy-data` (the CA + certs) stays, so the trusted root survives.
fn reseed(site: &Site, runtime: &str) {
    let _ = site.command(&[runtime, "rm", "-f", CADDY_CONTAINER]).output();
    let _ = site.command(&[runtime, "volume", "rm", CONFIG_VOLUME]).output();
}

/// Whether a rootless process may bind `port` at `site`. Where the sysctl can't
/// be read (non-Linux, or an unreadable knob) a privileged port reads as **not**
/// bindable: better to report a gap than to tear down a working Caddy for a port
/// that may not come up.
pub fn port_bindable(site: &Site, port: u16) -> bool {
    if port >= 1024 {
        return true;
    }
    let sysctl = "/proc/sys/net/ipv4/ip_unprivileged_port_start";
    let start = match site {
        Site::Local => std::fs::read_to_string(sysctl).ok(),
        Site::Remote { .. } => site
            .command(&["cat", sysctl])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned()),
    };
    start
        .and_then(|s| s.trim().parse::<u16>().ok())
        .map(|start| start <= port)
        .unwrap_or(false)
}

/// Bring up the shared routing infra for a workspace whose `domain` is set, and
/// report what `*.<domain>` answers — routes are live either way, but a browser
/// won't find them until the wildcard points at the right machine.
///
/// Both infra steps are **fatal** on failure: the generated override drops HTTP
/// host-ports, so Caddy is the only path to an HTTP service and a stack brought
/// "up" without it would be a lie.
pub fn ensure_routing(site: &Site, runtime: &str, domain: &str, publish_addr: &str) -> Result<Dns> {
    ensure_net(site, runtime).with_context(|| format!("routing needs the `{WS_NET}` network"))?;
    ensure_running(site, runtime, publish_addr)
        .context("caddy must be running to route HTTP services")?;
    Ok(resolve_domain(site, domain, expected_answer(publish_addr).as_deref()))
}

/// What `*.<domain>` should answer with, when it's cheaply known. `None` for the
/// bind-any default (`0.0.0.0` / `::`), where working the client's address out
/// costs an `ssh -G` — too much for the `ws up` hot path, so those callers fall
/// back to the coarse loopback/not-loopback question.
pub fn expected_answer(publish_addr: &str) -> Option<String> {
    publish_addr
        .trim()
        .parse::<std::net::IpAddr>()
        .ok()
        .filter(|ip| !ip.is_unspecified())
        .map(|ip| ip.to_string())
}

/// How to reach a route when the wildcard isn't answering the way this site
/// needs. The two failure shapes stay apart deliberately: "no answer" and "the
/// wrong answer" need different advice (see [`Dns`]).
pub fn resolve_hint(site: &Site, domain: &str, dns: &Dns) -> Option<String> {
    let port = https_port();
    match dns {
        Dns::Correct => None,
        Dns::Unresolved => Some(match site {
            Site::Local => format!(
                "`*.{domain}` doesn't resolve yet — reach a route with \
                 `curl --resolve <host>:{port}:127.0.0.1 https://<host>:{port}`"
            ),
            Site::Remote { .. } => format!(
                "`*.{domain}` doesn't resolve yet — point it at an address this server \
                 is reachable at (see `server-setup/<distro>.sh --address`), then reach a \
                 route on `https://<host>:{port}`"
            ),
        }),
        Dns::Wrong { got, expected: Some(want) } => Some(format!(
            "`*.{domain}` resolves to {got}, expected {want} — the wildcard points at the \
             wrong machine, so requests leave for a host this workspace no longer uses. \
             Re-run `ws setup` to re-point it."
        )),
        // Only reachable on a remote workspace with a bind-any publish address:
        // the wildcard resolved to loopback while the stacks are on the server.
        Dns::Wrong { got, expected: None } => Some(format!(
            "`*.{domain}` resolves to {got}, but this workspace's stacks run on the server — \
             point the wildcard at an address the server is reachable at \
             (see `server-setup/<distro>.sh --address`)"
        )),
    }
}

/// `<runtime> run -d` the Caddy container. Returns `None` on success and
/// `Some(reason)` on any failure, including a name clash with a peer's container.
///
/// `--resume` reloads the autosave when `/config` already holds one, so a
/// recreated container restores every worktree route; on a fresh box there is no
/// autosave and `--config` seeds the pinned boot config instead.
///
/// **Known gap:** that persistence targets a *Caddy-only* restart, where the
/// worktree containers are still up. After a host reboot they are gone but the
/// persisted routes remain, so a route to a dead upstream gives a 502 instead of
/// the friendly "run `ws up`" page (which only fires for hostnames with *no*
/// route). Re-running `ws up` re-homes the route and heals it.
fn run_caddy(site: &Site, runtime: &str, dir: &str, publish_addr: &str) -> Option<String> {
    let port = https_port();
    let publish = publish_spec(publish_addr, port);
    let admin = admin_port();
    // Published to the host loopback only, so the admin is reachable by `ws` and
    // not by the LAN. The container always listens on 2019 inside.
    let admin_publish = format!("127.0.0.1:{admin}:{DEFAULT_ADMIN_PORT}");
    let sock_mount = format!("{dir}:{SOCK_MOUNT}:Z");
    let data_vol = format!("{DATA_VOLUME}:/data");
    let config_vol = format!("{CONFIG_VOLUME}:/config");
    let config_json = format!("{SOCK_MOUNT}/caddy.json");
    let out = site.command(&[
        runtime, "run", "-d", "--name", CADDY_CONTAINER, "--network", WS_NET, "--restart", "always",
        "-p", &publish,
        "-p", &admin_publish,
        "-v", &sock_mount,
        "-v", &data_vol,
        "-v", &config_vol,
        CADDY_IMAGE,
        "caddy", "run", "--config", &config_json, "--resume",
    ])
    .output();
    // The reason is returned rather than printed so the caller surfaces it only on
    // the *terminal* failure — a first attempt that hits a stale name and recovers
    // on retry stays quiet.
    match out {
        Ok(o) if o.status.success() => None,
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            Some(crate::ui::first_nonempty_line(&err).unwrap_or("failed").to_string())
        }
        Err(e) => Some(e.to_string()),
    }
}

/// Caddy's admin listener isn't up the instant `run -d` returns, so the
/// immediately-following `route_up` would otherwise hit a dead socket.
fn wait_admin_ready(site: &Site) -> Result<()> {
    for _ in 0..ADMIN_POLLS {
        if curl(site, &["-o", "/dev/null"], "/config/").map(|o| o.ok).unwrap_or(false) {
            return Ok(());
        }
        std::thread::sleep(ADMIN_POLL_INTERVAL);
    }
    bail!("{CADDY_CONTAINER} started but its admin API never answered within 5s — it may be crash-looping (check the {CADDY_CONTAINER} container logs)")
}

/// What `*.<domain>` currently answers, measured against what the site needs.
///
/// Three states rather than a bool because "doesn't resolve" and "resolves to the
/// wrong machine" want different advice: a wildcard left pointing at a former
/// offload server used to read as *doesn't resolve yet*, sending readers hunting
/// for a missing DNS entry rather than a stale one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dns {
    /// NXDOMAIN, no resolver, or a lookup past the probe deadline.
    Unresolved,
    Correct,
    /// `got` is the first answer; `expected` is what we wanted, when it was
    /// cheaply known (see [`expected_answer`]).
    Wrong { got: String, expected: Option<String> },
}

impl Dns {
    pub fn is_ok(&self) -> bool {
        matches!(self, Dns::Correct)
    }
}

/// Resolve the wildcard `*.<domain>` and judge the answer.
///
/// Without an `expected` address the check falls back to the right *kind* — local
/// wants loopback, remote wants anything else — which still stops a resolver that
/// synthesizes an IP for every unknown name (ISP NXDOMAIN hijacking, a captive
/// portal) from reading as "resolved" locally.
///
/// The probe name usually does *not* resolve (that's what fires the warning), so
/// it runs on a scratch thread with a short deadline: a slow resolver must not
/// stall every `ws up` for seconds just to print a hint. A timeout counts as
/// [`Dns::Unresolved`], the safe default.
pub fn resolve_domain(site: &Site, domain: &str, expected: Option<&str>) -> Dns {
    // A label a real service is unlikely to shadow, so a hit means the wildcard
    // resolves rather than some coincidental host.
    let probe = format!("ws-dns-probe.{domain}:{}", https_port());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::net::ToSocketAddrs;
        let addrs: Vec<std::net::IpAddr> = probe
            .to_socket_addrs()
            .map(|a| a.map(|s| s.ip()).collect())
            .unwrap_or_default();
        let _ = tx.send(addrs); // receiver may have already timed out; ignore
    });
    let answers = rx.recv_timeout(std::time::Duration::from_millis(300)).unwrap_or_default();
    let Some(first) = answers.first() else { return Dns::Unresolved };

    let want = expected.and_then(|a| a.trim().parse::<std::net::IpAddr>().ok());
    if answers.iter().any(|a| answer_ok(a, want, !site.is_remote())) {
        return Dns::Correct;
    }
    Dns::Wrong { got: first.to_string(), expected: expected.map(str::to_string) }
}

/// A loopback expectation accepts *any* loopback answer: `::1` and `127.0.0.1`
/// are the same machine, which is the whole of what a local workspace asks for. A
/// concrete non-loopback expectation stays exact.
fn answer_ok(got: &std::net::IpAddr, expected: Option<std::net::IpAddr>, want_loopback: bool) -> bool {
    match expected {
        Some(e) if e.is_loopback() => got.is_loopback(),
        Some(e) => *got == e,
        None => got.is_loopback() == want_loopback,
    }
}

/// Externally-managed mode reports `true`: a BYO admin is "up" as far as `ws` is
/// concerned.
pub fn is_running(site: &Site, runtime: &str) -> bool {
    externally_managed() || container_running(site, runtime)
}

/// Fetch Caddy's internal root CA certificate (PEM), for the setup step that
/// trusts it in the system store.
pub fn root_ca_pem(site: &Site) -> Result<String> {
    let out = curl(site, &[], "/pki/ca/local")?;
    if !out.succeeded() {
        bail!("fetch Caddy root CA: HTTP {} {}", out.code, out.body.trim());
    }
    let v: serde_json::Value =
        serde_json::from_str(&out.body).context("parse /pki/ca/local response")?;
    v["root_certificate"]
        .as_str()
        .map(str::to_string)
        .context("no root_certificate in /pki/ca/local response")
}

/// Write the root CA to `~/.config/ws/ws-caddy-root.crt` (user-owned, 0600) and
/// return the path for the trust command. Deliberately not `$TMPDIR`/`/tmp`: a
/// predictable path in a world-writable dir is a clobber/symlink window before
/// the later `sudo` reads it.
pub fn write_root_ca(site: &Site) -> Result<PathBuf> {
    // Fetched from `site`, but written to the HOST config dir — the CA is trusted
    // in the *client's* store regardless of where the stack runs.
    let pem = root_ca_pem(site)?;
    let dir = crate::settings::global_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join("ws-caddy-root.crt");
    std::fs::write(&path, pem).with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(path)
}

fn container_running(site: &Site, runtime: &str) -> bool {
    site.command(&[runtime, "container", "inspect", "-f", "{{.State.Running}}", CADDY_CONTAINER])
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true")
        .unwrap_or(false)
}

/// Write the pinned boot config at `site` and return the dir mounted into the
/// container. The dir must be **absolute**: it is the bind-mount source for the
/// Caddy container, and the runtime rejects a relative one (which is what
/// `global_dir()` yields under an unset HOME).
fn write_boot_config(site: &Site) -> Result<String> {
    let dir: PathBuf = match site {
        Site::Local => {
            let dir = crate::settings::global_dir();
            std::fs::create_dir_all(&dir).ok();
            std::fs::canonicalize(&dir).unwrap_or(dir)
        }
        Site::Remote { .. } => {
            PathBuf::from(site.home_path(".config/ws").expect("remote site carries a home"))
        }
    };
    let dir = dir.to_string_lossy().into_owned();
    let json = serde_json::to_string_pretty(&boot_config())? + "\n";
    site.write_file(&format!("{dir}/caddy.json"), &json)?;
    Ok(dir)
}

/// The Caddy boot config.
///
/// **On-demand internal TLS is deliberate.** The catch-all matches hostnames `ws`
/// never registered, so Caddy has no ahead-of-time cert for them; `on_demand`
/// mints one at handshake time, turning a raw `tlsv1 internal error` into the
/// friendly page. Newer Caddy demands an `on_demand.permission` module first, but
/// that gate does not apply to the `internal` issuer (local and free, nothing to
/// exhaust) — verified against the pinned `caddy:2` v2.11.4. Do *not* "fix" this
/// by adding a permission endpoint.
///
/// **Seeds only a fresh box.** `run_caddy` passes `--resume`, so once an autosave
/// exists this config is ignored in favour of the persisted one — a change here
/// won't reach a box that already has a `ws-caddy` autosave.
fn boot_config() -> serde_json::Value {
    let port = https_port();
    serde_json::json!({
        // Must bind the container's routable interface, since a published port
        // DNATs there rather than to the container's loopback; the boundary is the
        // host publish being scoped to `127.0.0.1` (see `run_caddy`). An
        // `enforce_origin`/`origins` allow-list is not an option: Caddy's origin
        // check reads the `Origin` header, which `curl` doesn't send, so it would
        // reject `ws`'s own requests.
        "admin": { "listen": format!("0.0.0.0:{DEFAULT_ADMIN_PORT}") },
        "apps": {
            "http": {
                "https_port": port,
                "servers": {
                    SERVER: { "listen": [format!(":{port}")], "routes": [ stopped_stack_route() ] }
                }
            },
            "tls": { "automation": {
                "policies": [ { "issuers": [ { "module": "internal" } ], "on_demand": true } ]
            } }
        }
    })
}

/// The catch-all: no host match, so it matches everything. It must stay last in
/// the route list (worktree routes prepend, see `route_up`) or it would swallow
/// every live stack.
fn stopped_stack_route() -> serde_json::Value {
    let body = "<!doctype html><meta charset=utf-8>\
        <title>ws — no stack here</title>\
        <style>body{font-family:system-ui,sans-serif;max-width:34rem;margin:12vh auto;padding:0 1.5rem;line-height:1.5}\
        code{background:#eee;padding:.15em .4em;border-radius:.3em}</style>\
        <h1>No stack is running here</h1>\
        <p>Nothing is routed for this hostname yet. Bring its worktree up with:</p>\
        <p><code>ws up &lt;branch&gt;</code></p>";
    serde_json::json!({
        "@id": STOPPED_STACK_ID,
        "handle": [ {
            "handler": "static_response",
            "status_code": 503,
            "headers": { "Content-Type": [ "text/html; charset=utf-8" ] },
            "body": body
        } ]
    })
}

fn route(id: &str, hostname: &str, upstream: &str, wildcard: bool) -> serde_json::Value {
    // `*.foo` matches exactly one label and does NOT match bare `foo`, so a
    // wildcard service must list both. No wildcard *cert* is needed — on-demand
    // internal TLS mints one per tenant hostname at handshake.
    let hosts = if wildcard {
        vec![hostname.to_string(), format!("*.{hostname}")]
    } else {
        vec![hostname.to_string()]
    };
    serde_json::json!({
        "@id": id,
        "match": [ { "host": hosts } ],
        "handle": [ {
            "handler": "reverse_proxy",
            "upstreams": [ { "dial": upstream } ]
        } ]
    })
}

/// Add (or replace) the route for a worktree service. The prior route with the
/// same `@id` goes first so a re-`up` with a changed upstream takes effect, and
/// the new one lands at index 0 so it wins over the catch-all. Order among
/// worktree routes is irrelevant — each has a distinct host match.
pub fn route_up(site: &Site, spec: &RouteSpec) -> Result<()> {
    route_down(site, &spec.id);
    let body = route(&spec.id, &spec.hostname, &spec.upstream, spec.wildcard).to_string();
    let out = curl(
        site,
        &["-X", "PUT", "-H", "Content-Type: application/json", "-d", &body],
        &format!("/config/apps/http/servers/{SERVER}/routes/0"),
    )?;
    check(&out, &format!("add route {}", spec.id))
}

/// Infallible by design: teardown must survive a missing route or an unreachable
/// Caddy. A 404 is expected on a first up or a double down.
pub fn route_down(site: &Site, id: &str) {
    let _ = curl(site, &["-X", "DELETE"], &format!("/id/{id}/"));
}

/// Under an externally-managed Caddy the request runs **locally** whatever `site`
/// says: `WS_CADDY_ADMIN` is a host endpoint, not the server's admin port.
fn curl(site: &Site, extra: &[&str], path: &str) -> Result<CurlOut> {
    let local = Site::Local;
    let effective = if externally_managed() { &local } else { site };
    let url = admin_url(path);
    let mut argv: Vec<&str> = vec!["curl", "-sS", "-m", "10", "-w", "\n%{http_code}"];
    argv.extend(extra.iter().copied());
    argv.push(&url);
    let out = effective
        .command(&argv)
        .output()
        .context("spawn curl for Caddy admin API")?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, code) = text.rsplit_once('\n').unwrap_or((&text, ""));
    Ok(CurlOut {
        ok: out.status.success(),
        code: code.trim().parse().unwrap_or(0),
        body: body.to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    })
}

struct CurlOut {
    ok: bool,
    code: u16,
    body: String,
    stderr: String,
}

impl CurlOut {
    fn succeeded(&self) -> bool {
        self.ok && (200..300).contains(&self.code)
    }
}

fn check(out: &CurlOut, what: &str) -> Result<()> {
    if out.succeeded() {
        return Ok(());
    }
    let detail = if !out.body.is_empty() { &out.body } else { &out.stderr };
    bail!("{what}: HTTP {} {}", out.code, detail.trim());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote_site() -> Site {
        Site::Remote { connection: "server".into(), home: "/home/ws".into() }
    }

    #[test]
    fn publish_defaults_keep_local_private_and_make_remote_reachable() {
        // Loopback locally: a dev machine shouldn't serve its stacks to the LAN.
        assert_eq!(publish_address(&Site::Local, None), "127.0.0.1");
        // All interfaces remotely: loopback would make offload pointless, since
        // only the server itself could reach what it's serving.
        assert_eq!(publish_address(&remote_site(), None), "0.0.0.0");
    }

    #[test]
    fn a_configured_address_narrows_only_the_remote_site() {
        let cfg = Some("100.64.1.3");
        assert_eq!(publish_address(&remote_site(), cfg), "100.64.1.3");
        // Local ignores it: `ws setup`'s CA probe, `resolve_hint` and
        // `resolve_domain` all assume a local Caddy is on loopback, so honouring
        // it here would half-wire a workspace rather than configure it.
        assert_eq!(publish_address(&Site::Local, cfg), "127.0.0.1");
    }

    #[test]
    fn blank_publish_address_falls_back_to_the_default() {
        // A script exporting an unset var must not pin Caddy to an empty address.
        assert_eq!(publish_address(&remote_site(), Some("")), "0.0.0.0");
        assert_eq!(publish_address(&remote_site(), Some("   ")), "0.0.0.0");
    }

    #[test]
    fn publish_spec_brackets_ipv6_only() {
        assert_eq!(publish_spec("127.0.0.1", 8443), "127.0.0.1:8443:8443");
        assert_eq!(publish_spec("0.0.0.0", 8443), "0.0.0.0:8443:8443");
        // Unbracketed, the runtime can't tell the address from the port and
        // refuses the whole `-p` — stranding the narrowest, safest choice.
        assert_eq!(publish_spec("::1", 8443), "[::1]:8443:8443");
        assert_eq!(publish_spec("fd7a:115c::1", 443), "[fd7a:115c::1]:443:443");
    }

    #[test]
    fn route_id_is_stable_and_namespaced() {
        assert_eq!(route_id("feat-foo", "api", "web"), "ws-feat-foo-api-web");
        // Same service name in two repos → distinct ids (no cross-repo clobber).
        assert_ne!(route_id("feat-foo", "api", "web"), route_id("feat-foo", "ui", "web"));
        // Ordinary names survive untouched, including the underscores and digits
        // compose service names routinely carry.
        assert_eq!(route_id("release-1.2", "web_api", "app2"), "ws-release-1-2-web_api-app2");
    }

    #[test]
    fn route_id_cannot_escape_its_admin_path_segment() {
        // A cloned `.ws.json` names a service `/../../config`: interpolated raw,
        // `/id/<id>/` becomes a path curl normalizes down to `/config/`, so the
        // best-effort route delete would wipe the whole running Caddy config.
        let id = route_id("feat-foo", "api", "/../../config");
        assert!(!id.contains('/'), "id must stay one path segment: {id}");
        assert!(!id.contains(".."), "id must carry no dot segments: {id}");
        // Same for a hostile repo name, the other `.ws.json`-controlled part.
        let id = route_id("feat-foo", "../../..", "web");
        assert!(!id.contains('/') && !id.contains(".."), "{id}");
    }

    #[test]
    fn route_json_matches_host_and_proxies_upstream() {
        let r = route("ws-feat-foo-web", "feat-foo.web.myapp.test", "feat-foo-api-web-1:80", false);
        assert_eq!(r["@id"], "ws-feat-foo-web");
        assert_eq!(r["match"][0]["host"][0], "feat-foo.web.myapp.test");
        // A non-wildcard route matches exactly one host.
        assert_eq!(r["match"][0]["host"].as_array().unwrap().len(), 1);
        assert_eq!(r["handle"][0]["handler"], "reverse_proxy");
        assert_eq!(r["handle"][0]["upstreams"][0]["dial"], "feat-foo-api-web-1:80");
    }

    #[test]
    fn wildcard_route_matches_the_base_and_every_tenant_subdomain() {
        let r = route("ws-feat-foo-app", "feat-foo.app.myapp.test", "host.containers.internal:20001", true);
        let hosts: Vec<&str> =
            r["match"][0]["host"].as_array().unwrap().iter().map(|h| h.as_str().unwrap()).collect();
        // Both the bare host (no tenant) and the single-label tenant wildcard.
        // `*.foo` alone would NOT match bare `foo`, so both must be present.
        assert!(hosts.contains(&"feat-foo.app.myapp.test"), "missing base host: {hosts:?}");
        assert!(hosts.contains(&"*.feat-foo.app.myapp.test"), "missing tenant wildcard: {hosts:?}");
        assert_eq!(hosts.len(), 2);
    }

    #[test]
    fn boot_config_has_on_demand_internal_tls_and_the_catch_all() {
        // Hold the port-env lock + snapshot the port so a concurrent
        // `WS_HTTPS_PORT`-mutating test can't change it between the two reads.
        let _g = env_guard();
        let port = https_port();
        let c = boot_config();
        let policy = &c["apps"]["tls"]["automation"]["policies"][0];
        assert_eq!(policy["issuers"][0]["module"], "internal");
        // On-demand issuance so an unregistered hostname (a down stack) still gets
        // a cert for the stopped-stack page instead of a TLS error.
        assert_eq!(policy["on_demand"], true);
        assert_eq!(c["apps"]["http"]["servers"][SERVER]["listen"][0], format!(":{port}"));

        // The one boot route is the catch-all; worktree routes prepend ahead of it.
        let routes = c["apps"]["http"]["servers"][SERVER]["routes"].as_array().unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0]["@id"], STOPPED_STACK_ID);
    }

    #[test]
    fn stopped_stack_route_is_an_unmatched_static_page() {
        let r = stopped_stack_route();
        // No `match` ⇒ it catches every hostname that no worktree route claimed.
        assert!(r.get("match").is_none(), "catch-all must not filter by host: {r}");
        assert_eq!(r["handle"][0]["handler"], "static_response");
        assert_eq!(r["handle"][0]["status_code"], 503);
        assert!(r["handle"][0]["body"].as_str().unwrap().contains("ws up"));
    }

    #[test]
    fn https_port_env_overrides_the_default() {
        // The seam wins over the persisted file / default. (Serialized guard so a
        // parallel test reading the port isn't perturbed.)
        let _g = env_guard();
        std::env::set_var("WS_HTTPS_PORT", "443");
        assert_eq!(https_port(), 443);
        std::env::remove_var("WS_HTTPS_PORT");
        assert_eq!(https_port(), DEFAULT_HTTPS_PORT);
    }

    #[test]
    fn covers_matches_exact_or_wildcard_only() {
        assert!(covers("127.0.0.1", "127.0.0.1"));
        assert!(covers("0.0.0.0", "127.0.0.1"));
        assert!(covers("::", "100.64.0.5"));
        assert!(!covers("100.64.0.5", "127.0.0.1"));
        // The narrow side never covers the wide one.
        assert!(!covers("127.0.0.1", "0.0.0.0"));
    }

    #[test]
    fn port_suffix_is_empty_only_for_443() {
        assert_eq!(port_suffix(443), "");
        assert_eq!(port_suffix(8443), ":8443");
    }

    #[test]
    fn https_port_ignores_a_zero_value() {
        // A `0` env value (truncated file / bad input) must not publish port 0;
        // fall through to the default instead.
        let _g = env_guard();
        std::env::set_var("WS_HTTPS_PORT", "0");
        assert_eq!(https_port(), DEFAULT_HTTPS_PORT);
        std::env::remove_var("WS_HTTPS_PORT");
    }

    #[test]
    fn admin_port_seam_wins_and_zero_falls_through() {
        let _g = env_guard();
        std::env::set_var("WS_CADDY_ADMIN_PORT", "2020");
        assert_eq!(admin_port(), 2020);
        // A `0` must not publish port 0 — fall through like the HTTPS path.
        std::env::set_var("WS_CADDY_ADMIN_PORT", "0");
        assert_eq!(admin_port(), DEFAULT_ADMIN_PORT);
        std::env::remove_var("WS_CADDY_ADMIN_PORT");
        assert_eq!(admin_port(), DEFAULT_ADMIN_PORT);
    }

    #[test]
    fn set_ports_persist_under_their_own_file_names() {
        // Pins what the two setters share: file name per port, a trailing newline
        // so the file stays hand-editable, and that `persisted_port` reads it back.
        // Holds the env guard because it repoints the config dir process-wide.
        let _g = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", tmp.path());
        std::env::remove_var("WS_HTTPS_PORT");
        std::env::remove_var("WS_CADDY_ADMIN_PORT");

        // Values distinct from both defaults, so a no-op writer can't pass.
        set_https_port(9443).unwrap();
        set_admin_port(2021).unwrap();
        let dir = tmp.path().join("ws");
        assert_eq!(std::fs::read_to_string(dir.join("https_port")).unwrap(), "9443\n");
        assert_eq!(std::fs::read_to_string(dir.join("admin_port")).unwrap(), "2021\n");
        assert_eq!(https_port(), 9443);
        assert_eq!(admin_port(), 2021);

        match prev {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
    }

    #[test]
    fn parse_listen_port_reads_the_served_port() {
        assert_eq!(parse_listen_port(r#"[":8443"]"#), Some(8443));
        assert_eq!(parse_listen_port(r#"["127.0.0.1:443"]"#), Some(443));
        assert_eq!(parse_listen_port("[]"), None);
        assert_eq!(parse_listen_port("not json"), None);
    }

    #[test]
    fn port_bindable_gates_privileged_ports() {
        // Non-privileged ports are always bindable.
        assert!(port_bindable(&Site::Local, 8443));
        assert!(port_bindable(&Site::Local, 1024));
        // A privileged port depends on the sysctl; on the test host (which we
        // can't assume has it lowered) the result must at least be a clean bool,
        // and 443 is bindable iff ip_unprivileged_port_start <= 443.
        let start = std::fs::read_to_string("/proc/sys/net/ipv4/ip_unprivileged_port_start")
            .ok()
            .and_then(|s| s.trim().parse::<u16>().ok());
        if let Some(start) = start {
            assert_eq!(port_bindable(&Site::Local, 443), start <= 443);
        }
    }

    /// Serialize the env-mutating port tests (they share the process env) and
    /// point the global config dir at an empty scratch dir — otherwise a machine
    /// that opted into a non-default port via `ws setup --port` reads its own
    /// `~/.config/ws/{https,admin}_port` and fails the defaults.
    fn env_guard() -> EnvGuard {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("scratch config dir");
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", dir.path());
        EnvGuard { prev, _dir: dir, _lock: lock }
    }

    struct EnvGuard {
        prev: Option<std::ffi::OsString>,
        _dir: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for EnvGuard {
        // Runs before the fields, so the lock is still held while the env is put
        // back — a waiting test can't observe the scratch dir.
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
    }

    #[test]
    fn admin_listens_on_the_admin_port_over_tcp() {
        // TCP on the admin port (a unix socket can't cross the macOS podman VM),
        // bound to the routable interface so the loopback-scoped publish reaches it.
        assert_eq!(boot_config()["admin"]["listen"], format!("0.0.0.0:{DEFAULT_ADMIN_PORT}"));
    }

    #[test]
    fn expected_answer_is_known_for_a_concrete_publish_address_only() {
        // Local always publishes loopback, so the exact answer is free.
        assert_eq!(expected_answer("127.0.0.1").as_deref(), Some("127.0.0.1"));
        // A pinned remote publish address is both the bind and the dial address.
        assert_eq!(expected_answer("100.64.1.3").as_deref(), Some("100.64.1.3"));
        // Bind-any is not an address anything resolves *to*; deriving the real one
        // needs an `ssh -G`, which must not land on the `ws up` hot path.
        assert_eq!(expected_answer("0.0.0.0"), None);
        assert_eq!(expected_answer("::"), None);
        assert_eq!(expected_answer(""), None);
    }

    #[test]
    fn a_loopback_expectation_accepts_any_loopback_answer() {
        use std::net::IpAddr;
        let v4: IpAddr = "127.0.0.1".parse().unwrap();
        let v6: IpAddr = "::1".parse().unwrap();
        // `::1` and `127.0.0.1` are the same machine — a local workspace asks for
        // no more than that, so requiring literal equality would report a gap
        // where there is none.
        assert!(answer_ok(&v6, Some(v4), true));
        assert!(answer_ok(&v4, Some(v6), true));
        // A concrete non-loopback expectation is exact: that precision is the
        // whole reason the address is threaded through.
        let srv: IpAddr = "100.64.1.3".parse().unwrap();
        let other: IpAddr = "100.64.1.2".parse().unwrap();
        assert!(answer_ok(&srv, Some(srv), false));
        assert!(!answer_ok(&other, Some(srv), false));
        // With nothing expected, only the *kind* of address is checked.
        assert!(answer_ok(&v4, None, true));
        assert!(!answer_ok(&srv, None, true));
        assert!(answer_ok(&srv, None, false));
    }

    #[test]
    fn the_hint_says_what_it_resolved_to_not_merely_that_it_didnt() {
        // The incident: a workspace flipped back to local while `*.<domain>` still
        // answered with the offload server's tailnet address. The old wording
        // ("doesn't resolve yet") sent the reader looking for a missing DNS entry
        // instead of a stale one.
        let dns = Dns::Wrong {
            got: "100.64.1.3".to_string(),
            expected: Some("127.0.0.1".to_string()),
        };
        let hint = resolve_hint(&Site::Local, "myapp.test", &dns).expect("a wrong answer hints");
        assert!(hint.contains("resolves to 100.64.1.3"), "{hint}");
        assert!(hint.contains("expected 127.0.0.1"), "{hint}");
        assert!(!hint.contains("doesn't resolve"), "wrong answer is not a missing one: {hint}");

        // Not resolving at all keeps its own, distinct advice.
        let missing = resolve_hint(&Site::Local, "myapp.test", &Dns::Unresolved).unwrap();
        assert!(missing.contains("doesn't resolve yet"), "{missing}");
        assert!(missing.contains("curl --resolve"), "{missing}");

        // And a correct wildcard says nothing at all.
        assert_eq!(resolve_hint(&Site::Local, "myapp.test", &Dns::Correct), None);
    }

    #[test]
    fn a_name_that_cannot_resolve_reads_as_unresolved_not_wrong() {
        // `.invalid` is reserved by RFC 2606, so this is deterministic offline.
        assert_eq!(
            resolve_domain(&Site::Local, "ws-no-such-domain.invalid", Some("127.0.0.1")),
            Dns::Unresolved
        );
    }

    #[test]
    fn admin_url_hits_the_loopback_admin_port() {
        // Reads `admin_port()` twice (inside `admin_url` and in the expectation),
        // so it must hold the env guard — else a concurrent `WS_CADDY_ADMIN_PORT`
        // mutation between the two reads makes them disagree.
        let _g = env_guard();
        assert_eq!(admin_url("/config/"), format!("http://127.0.0.1:{}/config/", admin_port()));
    }
}
