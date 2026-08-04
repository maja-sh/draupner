//! Containerized Caddy on the shared `ws-net`, driven by its admin API.
//!
//! Research-settled topology: a rootless *host* Caddy can't resolve container
//! DNS (aardvark/netavark's resolver lives inside the container netns), so Caddy
//! runs as a **container** attached to `ws-net` and dials each worktree's
//! containers by their project-qualified name `<project>-<service>-1`. Host
//! processes are reached at `host.containers.internal:<port>`.
//!
//! Route lifecycle goes through Caddy's **admin API**, each route addressed by a
//! stable `@id` = `ws-<branch>-<service>`: `ws up` adds it, `down`/`rm` delete
//! it. We never `POST /load` (that replaces the whole config and would clobber
//! other worktrees' routes); adding is a scoped append and deleting is by `@id`,
//! so parallel `ws` processes touching distinct ids don't race.
//!
//! **HTTPS port:** Caddy binds a *non-privileged* port by default (`8443`), so
//! TLS works with zero setup — reach a route with
//! `curl --resolve <host>:8443:127.0.0.1 https://<host>:8443`. `:443` (clean,
//! suffix-free browser URLs) is an opt-in: it needs the one-time
//! `net.ipv4.ip_unprivileged_port_start` sysctl, and `ws setup --port 443`
//! persists the choice + re-seeds Caddy. See `https_port` / `ensure_running`.
//!
//! **Admin API on a host-loopback-published TCP port.** `ws` reaches the admin at
//! `127.0.0.1:2019` by default, published from the container to the host loopback
//! only (`-p 127.0.0.1:2019:2019`). The host port is configurable
//! (`ws setup --admin-port`) for a box already running its own Caddy on 2019 —
//! the container always listens on 2019 inside. See [`admin_port`]. A unix socket would be tighter, but it can't cross
//! the podman-machine VM boundary on macOS (the socket lives in the VM, not on the
//! Mac), so `curl --unix-socket` is Linux-only — this TCP path is the one form that
//! works identically on macOS and Linux. The publish is loopback-scoped, so the
//! admin is unreachable from the host's LAN — that scoping is the only boundary
//! (an `origins` allow-list can't be used: Caddy's origin check reads the `Origin`
//! header, which `curl` doesn't send, so it would reject `ws`'s own requests).
//! Residual: because a published port must bind the container's
//! routable interface, a tenant on `ws-net` could in principle reach `caddy:2019` —
//! acceptable for a local dev tool (tenants are your own stacks); a dedicated
//! admin network is the hardening follow-up. See `boot_config`/`admin_args`.
//!
//! Seams: `WS_CADDY_ADMIN` (when set, `ws` talks to that admin URL and treats
//! Caddy + `ws-net` as externally managed — BYO / test stub) and the runtime
//! binary let tests substitute fakes.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

use crate::remote::Site;

/// Shared user-defined network every worktree's stack and Caddy join.
pub const WS_NET: &str = "ws-net";
/// Container name for the singleton Caddy.
pub const CADDY_CONTAINER: &str = "ws-caddy";
/// Pinned Caddy image.
const CADDY_IMAGE: &str = "docker.io/library/caddy:2";
/// The HTTP server block name inside Caddy's config (routes hang off it).
const SERVER: &str = "ws";
/// Default HTTPS listen port — **non-privileged**, so Caddy binds it with no
/// setup. TLS works here today (`curl --resolve <host>:8443:127.0.0.1 …`); the
/// only thing `:443` buys is dropping the `:8443` suffix from browser URLs, and
/// that needs the one privileged `net.ipv4.ip_unprivileged_port_start` sysctl.
/// So `:443` is an **opt-in** (`ws setup --port 443`, persisted below); this
/// default keeps the rootless path zero-privilege.
pub const DEFAULT_HTTPS_PORT: u16 = 8443;
/// Mount point inside the Caddy container holding the pinned boot config
/// (`caddy.json`), bind-mounted from `config_dir()`.
const SOCK_MOUNT: &str = "/sock";
/// Caddy's conventional admin port, and `ws`'s default.
///
/// **Not a constant in practice**: a host that already runs its own Caddy is
/// holding this exact port, since 2019 is *Caddy's* default too — not merely a
/// port `ws` happened to pick. The original "a singleton per host" assumption
/// only holds when `ws` owns the box's only Caddy. See [`admin_port`].
const DEFAULT_ADMIN_PORT: u16 = 2019;
/// Named volume for Caddy's data dir (`/data`): the internal CA root + issued
/// leaf certs. Persisting it keeps the root stable across container recreation
/// (so a client trusts it once) and avoids reissuing every leaf on restart.
const DATA_VOLUME: &str = "ws-caddy-data";
/// Named volume for Caddy's config dir (`/config`): where `caddy run --resume`
/// reads and writes its autosave. Persisting it means a recreated `ws-caddy`
/// reloads every worktree route from its last-known config instead of coming up
/// empty (routes otherwise live only in Caddy's in-memory admin config).
const CONFIG_VOLUME: &str = "ws-caddy-config";
/// The `@id` of the catch-all "stopped stack" route (a hostname with no live
/// worktree route falls through to it). Distinct from any `route_id`, so route
/// CRUD for worktree services never touches it.
const STOPPED_STACK_ID: &str = "ws-stopped-stack";
/// The name a route for a `kind: host` repo dials to reach the host process —
/// podman's own alias for the machine the containers run on. Owned here because
/// Caddy is the container that has to resolve it (see [`host_alias_args`]).
pub const HOST_ALIAS: &str = "host.containers.internal";

/// The curl args to reach the admin API for `path`: the host-loopback-published
/// TCP admin (default), or the `WS_CADDY_ADMIN` URL verbatim (BYO / test seam).
/// For a remote site the admin is published on the *server's* loopback and `curl`
/// itself runs there (see [`curl`]), so `127.0.0.1` resolves server-side.
fn admin_args(_site: &Site, path: &str) -> Vec<String> {
    if let Ok(url) = std::env::var("WS_CADDY_ADMIN") {
        vec![format!("{}{}", url.trim_end_matches('/'), path)]
    } else {
        vec![format!("http://127.0.0.1:{}{path}", admin_port())]
    }
}

/// Whether Caddy + `ws-net` are managed outside `ws` — signalled by an explicit
/// `WS_CADDY_ADMIN`. In that mode `ws` still adds/removes routes against the
/// given admin API but does not create the network or run the container (you
/// bring your own). Tests set it to a dead endpoint so a sweep never spins up
/// real infra. Public so ops can ask "is there real infra to reason about?"
/// without naming the seam env var themselves.
pub fn externally_managed() -> bool {
    std::env::var_os("WS_CADDY_ADMIN").is_some()
}

/// The HTTPS port Caddy publishes and serves on. `WS_HTTPS_PORT` (test seam)
/// wins, else the persisted opt-in in `~/.config/ws/https_port` (written by
/// `ws setup --port`), else the non-privileged default. A change here only takes
/// effect once `ensure_running` re-seeds — `--resume` otherwise pins the old
/// port from the autosave.
pub fn https_port() -> u16 {
    // A `0` (or unparseable) value would publish/listen on port 0 — never usable.
    // Ignore it and fall through to the default, so a truncated file / bad env
    // can't seed a broken Caddy.
    if let Some(p) = std::env::var("WS_HTTPS_PORT").ok().and_then(|v| v.trim().parse().ok()) {
        if p != 0 {
            return p;
        }
    }
    std::fs::read_to_string(config_dir().join("https_port"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .filter(|&p| p != 0)
        .unwrap_or(DEFAULT_HTTPS_PORT)
}

/// The `:port` suffix a browser URL carries, empty for the standard `:443`.
pub fn port_suffix(port: u16) -> String {
    if port == 443 {
        String::new()
    } else {
        format!(":{port}")
    }
}

/// The address Caddy publishes its HTTPS port on. Local is always loopback;
/// remote honours `remote.publish_address` and otherwise takes all interfaces.
/// The value is validated at `config::load`, so this can't fail.
/// See [`crate::config::RemoteConfig::publish_address`] for the user-facing rules.
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

/// The `-p` argument publishing `port` on `addr`. IPv6 hosts must be bracketed or
/// the runtime can't tell the address from the port — an unbracketed `::1` is
/// rejected outright, which would strand exactly the user narrowing their exposure.
fn publish_spec(addr: &str, port: u16) -> String {
    match addr.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(a)) => format!("[{a}]:{port}:{port}"),
        _ => format!("{addr}:{port}:{port}"),
    }
}

/// The host address the running Caddy actually publishes on, via
/// `<runtime> port <container> <port>` (`0.0.0.0:8443` → `0.0.0.0`). `None` when
/// the container isn't running or the output isn't the expected shape — callers
/// treat that as "can't tell", never as a mismatch.
fn published_host(site: &Site, runtime: &str, port: u16) -> Option<String> {
    let out = site
        .command(&[runtime, "port", CADDY_CONTAINER, &port.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout).lines().next()?.trim().to_string();
    // `0.0.0.0:8443` / `[::1]:8443` — split at the last colon to keep IPv6 intact.
    let host = line.rsplit_once(':')?.0.to_string();
    (!host.is_empty()).then(|| host.trim_matches(['[', ']']).to_string())
}

/// Persist the opt-in HTTPS port so every later `ws` run agrees on it. Caddy
/// keeps serving the old port until the next `ensure_running` re-seeds it.
pub fn set_https_port(port: u16) -> Result<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::write(dir.join("https_port"), format!("{port}\n"))
        .with_context(|| "persist https_port".to_string())
}

/// The admin API port `ws` publishes to the site's loopback. `WS_CADDY_ADMIN_PORT`
/// (test seam) wins, else the persisted opt-in in `~/.config/ws/admin_port`
/// (written by `ws setup --admin-port`), else Caddy's conventional default.
///
/// Configurable for the same reason `https_port` is, but a sharper one: an offload
/// server that already runs its own Caddy holds 2019 *because that is Caddy's
/// default*, so the collision is likely rather than unlucky. Every route CRUD call
/// dials this, so it must be stable across runs — hence persisted, never picked
/// fresh per invocation.
pub fn admin_port() -> u16 {
    if let Some(p) = std::env::var("WS_CADDY_ADMIN_PORT").ok().and_then(|v| v.trim().parse().ok()) {
        if p != 0 {
            return p;
        }
    }
    std::fs::read_to_string(config_dir().join("admin_port"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .filter(|&p| p != 0)
        .unwrap_or(DEFAULT_ADMIN_PORT)
}

/// Persist the opt-in admin port so every later `ws` run dials the same one.
pub fn set_admin_port(port: u16) -> Result<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::write(dir.join("admin_port"), format!("{port}\n"))
        .with_context(|| "persist admin_port".to_string())
}

/// A route's stable admin `@id` for a worktree service. Includes the repo so
/// two repos in one branch that expose a same-named service (e.g. both `web`)
/// don't collide on one `@id` and delete each other's route.
pub fn route_id(branch_sanitized: &str, repo: &str, service: &str) -> String {
    format!("ws-{branch_sanitized}-{repo}-{service}")
}

/// A worktree HTTP service's Caddy route: its `@id`, the hostname Caddy matches,
/// and the upstream it proxies to (`<project>-<svc>-1:<port>` for a container,
/// `host.containers.internal:<port>` for a host process).
pub struct RouteSpec {
    pub id: String,
    pub hostname: String,
    pub upstream: String,
    /// Also match `*.<hostname>` (every tenant subdomain), not just `<hostname>`.
    pub wildcard: bool,
}

/// Refuse to drive a runtime that isn't installed, before the first spawn turns
/// it into `No such file or directory`.
///
/// **Local sites only.** A remote site's binaries live on the server, and this
/// looks at the client's PATH — checking there would refuse a perfectly good
/// offload workspace because the laptop lacks the server's runtime.
fn runtime_available(site: &Site, runtime: &str) -> Result<()> {
    if site.is_remote() {
        return Ok(());
    }
    crate::config::ensure_available(runtime)
}

/// Ensure the shared `ws-net` exists (idempotent rootless create) at `site`.
pub fn ensure_net(site: &Site, runtime: &str) -> Result<()> {
    if externally_managed() {
        return Ok(());
    }
    runtime_available(site, runtime)?;
    let out = site
        .command(&[runtime, "network", "create", WS_NET])
        .output()
        .with_context(|| format!("spawn `{runtime} network create`"))?;
    if out.status.success() {
        return Ok(());
    }
    // `network create` on an existing network is the common, benign case
    // (concurrent `ws up`s race to create it). Match case-insensitively so both
    // podman ("...already exists") and docker phrasings are covered.
    let err = String::from_utf8_lossy(&out.stderr);
    let lower = err.to_lowercase();
    if lower.contains("already") || lower.contains("exists") {
        return Ok(());
    }
    bail!("create {WS_NET}: {}", err.trim());
}

/// Ensure the singleton Caddy container is running, booting it from a pinned
/// config if not. Idempotent (a no-op when already running or externally managed).
///
/// Race-safe against parallel `ws up`: it **never removes a running container**
/// — doing so would delete a peer session's live Caddy and its in-memory routes.
/// It tries to run; a name clash from a peer that already has it up is success;
/// only a stale *stopped* container is removed and retried. (A brief residual
/// TOCTOU between the not-running check and the `rm` is accepted — the common
/// paths, already-running and fresh-box, are safe.)
pub fn ensure_running(site: &Site, runtime: &str, publish_addr: &str) -> Result<()> {
    if externally_managed() {
        return Ok(());
    }
    runtime_available(site, runtime)?;
    if container_running(site, runtime, CADDY_CONTAINER) {
        // A running Caddy is normally left strictly alone (its live routes are the
        // whole point). The one exception is a deliberate **port move**: `--resume`
        // pins the port from the persisted autosave, so `ws setup --port 443` only
        // takes effect if we re-seed. We reseed **only** when the port Caddy is
        // actually serving differs from the desired one — comparing the live port,
        // not a recorded marker, so a routine `ws up` (or a `ws` upgrade that
        // didn't change the port) never needlessly tears a peer's routes down.
        match served_port(site) {
            // Right port — but the publish *address* can still have changed, and
            // it's invisible to Caddy (a `-p` binding is the runtime's business,
            // not the proxy's), so `served_port` can never reveal it. Without this
            // check, pinning `remote.publish_address` to close an exposure would
            // silently keep the old binding, and `--restart always` would carry it
            // across reboots while the user believed it was fixed.
            Some(p) if p == https_port() => {
                match published_host(site, runtime, p) {
                    Some(h) if h != publish_addr => {
                        crate::ui::note(&format!(
                            "re-creating {CADDY_CONTAINER} to publish on {publish_addr} \
                             (currently {h}) — routes are preserved"
                        ));
                        // Cheaper than `reseed`: only the container is replaced, so
                        // `--resume` reloads every peer's route from the retained
                        // config volume. A port move needs the wipe; this doesn't.
                        let _ = site.command(&[runtime, "rm", "-f", CADDY_CONTAINER]).output();
                    }
                    // Matching → never destroy a working Caddy.
                    _ => return Ok(()),
                }
            }
            // Admin unreachable on a *running* container: it predates the switch to
            // a TCP admin (its unix-socket admin isn't on :2019) or is wedged. Its
            // autosaved config would also restore the old admin under `--resume`, so
            // reseed (drops the config volume) and start fresh — rather than
            // silently believing an unadministrable Caddy is healthy.
            None => {
                crate::ui::note(&format!(
                    "{CADDY_CONTAINER} is running but its admin is unreachable — \
                     re-seeding it (routes reinstall on `ws up`)"
                ));
                reseed(site, runtime);
            }
            // Port differs. Only reseed if the new port can actually bind — else
            // we'd tear down a working Caddy for one that fails to start (e.g.
            // `:443` before the sysctl), leaving nothing running. Keep the old one
            // and let `ws setup` report the gap.
            Some(_) if !port_bindable(site, https_port()) => return Ok(()),
            Some(_) => {
                crate::ui::note(&format!(
                    "re-seeding {CADDY_CONTAINER} for the :{} move — routes reinstall on `ws up`",
                    https_port()
                ));
                reseed(site, runtime);
            }
        }
    }
    ensure_net(site, runtime)?;

    // Pre-flight the two ports Caddy must bind before attempting the run. A busy
    // port otherwise surfaces as a raw `rootlessport ... address already in use`
    // and a half-created container — unactionable. This is exactly the failure a
    // box already running its own Caddy hits: it holds :2019 (Caddy's own default
    // admin port), and often an HTTPS port too. Check at the site, and name the
    // flag that moves each. A running `ws-caddy` (ours or a peer's) is handled
    // above and returns before here, so this only pre-flights against a *foreign*
    // holder. The narrow race — a peer starting `ws-caddy` between that check and
    // this one — would misreport a benign peer as a conflict; it's the same brief
    // TOCTOU the retry-once logic below already accepts, not a new one.
    preflight_ports(site)?;

    let dir = write_boot_config(site)?;
    if run_caddy(site, runtime, &dir, publish_addr).is_none() {
        return wait_admin_ready(site);
    }
    // The run failed — either a peer already started it (name taken, now
    // running → done) or a stale stopped container holds the name (remove it,
    // never a running one, and retry once).
    if container_running(site, runtime, CADDY_CONTAINER) {
        return wait_admin_ready(site);
    }
    let _ = site.command(&[runtime, "rm", "-f", CADDY_CONTAINER]).output();
    let reason = run_caddy(site, runtime, &dir, publish_addr);
    if reason.is_none() || container_running(site, runtime, CADDY_CONTAINER) {
        return wait_admin_ready(site);
    }
    // Terminal failure: surface the real reason (e.g. `:2019`/`:8443` in use) —
    // the container never started, so `podman logs ws-caddy` would be empty.
    bail!(
        "could not start the {CADDY_CONTAINER} container: {}",
        reason.unwrap_or_else(|| "unknown error".to_string())
    )
}

/// Refuse to attempt a Caddy run when either port it must publish is already held
/// at `site`, with a message that names the flag to move it. Returns the first
/// conflict as an error; `Ok(())` when both are free (or freeness can't be
/// determined, which `port_free_at` treats as busy only on a positive signal).
///
/// Under `WS_CADDY_ADMIN` (externally-managed) `ws` publishes nothing itself, so
/// there's nothing to pre-flight — but `ensure_running` already returns early in
/// that mode before reaching here.
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

/// The HTTPS port the running Caddy is actually listening on, read from its admin
/// config (`.../servers/ws/listen` = `[":8443"]`). `None` when the admin can't be
/// reached or the shape is unexpected — callers treat that as "don't touch it".
fn served_port(site: &Site) -> Option<u16> {
    let out = curl(site, &admin_args(site, &format!("/config/apps/http/servers/{SERVER}/listen"))).ok()?;
    if !out.ok || !(200..300).contains(&out.code) {
        return None;
    }
    parse_listen_port(&out.body)
}

/// Pull the port out of Caddy's `listen` array JSON (`[":8443"]` /
/// `["127.0.0.1:8443"]`) — the segment after the last colon of the first entry.
fn parse_listen_port(body: &str) -> Option<u16> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get(0)?.as_str()?.rsplit(':').next()?.parse().ok()
}

/// Tear down a Caddy serving the wrong port so a fresh boot config can seed the
/// new one. Removes the container *and* the config volume — the autosave
/// `--resume` would otherwise reload lives in that volume, and it's what pins the
/// old port. Routes live there too, so they're reinstalled on the next `ws up`
/// (acceptable for a deliberate, one-time port move). Keeps `ws-caddy-data` (the
/// CA + certs) so the root stays stable and trusted.
fn reseed(site: &Site, runtime: &str) {
    let _ = site.command(&[runtime, "rm", "-f", CADDY_CONTAINER]).output();
    let _ = site.command(&[runtime, "volume", "rm", CONFIG_VOLUME]).output();
}

/// Whether a rootless process may bind `port` on this host. Privileged ports
/// (`< 1024`) need `net.ipv4.ip_unprivileged_port_start` lowered on Linux; on a
/// host where we can't verify that (non-Linux, or the knob is unreadable) a
/// privileged port reads as **not** bindable — better to report a gap than to
/// tear down a working Caddy (or claim "ready") for a port that may not bind.
/// The single owner of this check, shared by `ensure_running` and the setup
/// probe. Checks the sysctl at `site` (the server's `/proc` for a remote stack).
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

/// Bring up the shared routing infra for a workspace whose `domain` is set:
/// create `ws-net`, start Caddy, then probe DNS. Returns what `*.<domain>`
/// currently answers ([`Dns`]) — routes are live either way, but a browser won't
/// find them until the wildcard points at the right machine. Both infra steps are
/// **fatal** on failure: since the override drops HTTP host-ports (`!reset`),
/// Caddy is the only path to an HTTP service, so a stack brought "up" without it
/// would be a lie. The one owner of the routing bring-up sequence — callers decide
/// only whether to surface [`resolve_hint`].
pub fn ensure_routing(site: &Site, runtime: &str, domain: &str, publish_addr: &str) -> Result<Dns> {
    ensure_net(site, runtime).with_context(|| format!("routing needs the `{WS_NET}` network"))?;
    ensure_running(site, runtime, publish_addr)
        .context("caddy must be running to route HTTP services")?;
    Ok(resolve_domain(site, domain, expected_answer(publish_addr).as_deref()))
}

/// What `*.<domain>` should answer with, when it's cheaply known — derived from
/// where Caddy publishes. Loopback for a local workspace; the pinned
/// `remote.publish_address` for a remote one (a concrete pin is both the bind
/// address and the address the client dials, see `ops::setup::routing_address`).
///
/// `None` for the bind-any default (`0.0.0.0` / `::`), where working the client's
/// address out costs an `ssh -G` — too much for the `ws up` hot path, so those
/// callers fall back to the coarse loopback/not-loopback question.
pub fn expected_answer(publish_addr: &str) -> Option<String> {
    publish_addr
        .trim()
        .parse::<std::net::IpAddr>()
        .ok()
        .filter(|ip| !ip.is_unspecified())
        .map(|ip| ip.to_string())
}

/// The recipe for reaching a route when the wildcard isn't answering the way this
/// site needs — the single owner of these user-facing strings (also referenced in
/// the module doc). `None` when DNS is already right.
///
/// The two failure shapes are kept apart deliberately: "no answer" and "the wrong
/// answer" need different advice, and conflating them is what made a stale
/// offload-server address read as `doesn't resolve yet` on a workspace that had
/// been flipped back to local.
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
        // Only reachable on a remote workspace with a bind-any publish address, where
        // the exact answer isn't cheaply knowable — so the wildcard resolved to
        // loopback and the stacks are on the server.
        Dns::Wrong { got, expected: None } => Some(format!(
            "`*.{domain}` resolves to {got}, but this workspace's stacks run on the server — \
             point the wildcard at an address the server is reachable at \
             (see `server-setup/<distro>.sh --address`)"
        )),
    }
}

/// `<runtime> run -d` the Caddy container. `dir` (the host config dir) is mounted
/// so Caddy reads its boot config; two named volumes persist Caddy's data
/// (`/data`: internal CA + certs) and config (`/config`: the `--resume` autosave)
/// across recreation. Both the HTTPS data-plane port and the admin port are
/// published; the admin publish is scoped to the host loopback.
///
/// `--resume` reloads the autosaved config when `/config` already holds one (so a
/// recreated container restores every worktree route); on a fresh box there's no
/// autosave and `--config` seeds the pinned boot config instead. Returns whether
/// it started; a name clash (a peer's container) or any other failure is `false`,
/// not an error.
///
/// **Known trade-off (persistence vs the stopped-stack page):** persistence is
/// aimed at a *Caddy-only* restart (the worktree containers stay up, so restoring
/// their routes keeps them reachable). After a full host reboot the worktree
/// containers are usually gone while Caddy's persisted routes remain, so a
/// restored route to a now-dead upstream yields a 502 rather than the friendly
/// "run `ws up`" page (which only fires for hostnames with *no* route). Re-running
/// `ws up` re-homes the route to the live container and heals it. A future
/// refinement (health-check the upstream, or reconcile routes against live
/// containers on boot) could close the gap.
/// The `run` args that make [`HOST_ALIAS`] resolve inside the Caddy container.
///
/// Podman defines the name itself, so it needs nothing. Docker defines *neither*
/// it nor its own `host.docker.internal` on Linux, so every `kind: host` route
/// 502s without this — Caddy resolves the upstream, not `ws`, and Caddy is the
/// one container `ws` runs, so this is the only place that can fix it.
/// `host-gateway` is the runtime's own placeholder for the host's address.
///
/// Keyed on "not podman" rather than "is docker" so an unrecognized runtime gets
/// the alias too: podman tolerates the flag (its native answer wins), which makes
/// adding it the safe side of the guess.
fn host_alias_args(runtime: &str) -> Vec<&'static str> {
    /// [`HOST_ALIAS`] mapped to the host — spelled out because `--add-host` takes
    /// one `name:addr` token and there's no const concatenation. A test pins it
    /// to `HOST_ALIAS` so the two can't drift.
    const ALIAS_TO_GATEWAY: &str = "host.containers.internal:host-gateway";

    if runtime.ends_with("podman") {
        return Vec::new();
    }
    vec!["--add-host", ALIAS_TO_GATEWAY]
}

fn run_caddy(site: &Site, runtime: &str, dir: &str, publish_addr: &str) -> Option<String> {
    let port = https_port();
    let publish = publish_spec(publish_addr, port);
    // Admin published to the host loopback only — reachable by `ws`, not the LAN.
    let admin = admin_port();
    // Published to loopback only; the container always listens on 2019 inside.
    let admin_publish = format!("127.0.0.1:{admin}:{DEFAULT_ADMIN_PORT}");
    let sock_mount = format!("{dir}:{SOCK_MOUNT}:Z");
    let data_vol = format!("{DATA_VOLUME}:/data");
    let config_vol = format!("{CONFIG_VOLUME}:/config");
    let config_json = format!("{SOCK_MOUNT}/caddy.json");
    let mut args = vec![
        runtime, "run", "-d", "--name", CADDY_CONTAINER, "--network", WS_NET, "--restart", "always",
    ];
    args.extend(host_alias_args(runtime));
    args.extend([
        "-p", &publish,
        "-p", &admin_publish,
        "-v", &sock_mount,
        "-v", &data_vol,
        "-v", &config_vol,
        CADDY_IMAGE,
        "caddy", "run", "--config", &config_json, "--resume",
    ]);
    let out = site.command(&args).output();
    // `None` on success; `Some(reason)` on failure. The reason is returned rather
    // than printed so the caller surfaces it only on the *terminal* failure — a
    // first attempt that hits a stale-name and recovers on retry stays quiet.
    match out {
        Ok(o) if o.status.success() => None,
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            Some(err.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("failed").to_string())
        }
        Err(e) => Some(e.to_string()),
    }
}

/// Poll the admin API until it answers — Caddy's listener isn't up the instant
/// `run -d` returns, so the immediately-following `route_up` could otherwise hit
/// a not-yet-listening socket. Fails with a clear message if it never answers
/// within the budget (a started-but-wedged Caddy, so routing can't proceed).
fn wait_admin_ready(site: &Site) -> Result<()> {
    for _ in 0..50 {
        let mut args = vec!["-o".to_string(), "/dev/null".to_string()];
        args.extend(admin_args(site, "/config/"));
        if curl(site, &args).map(|o| o.ok).unwrap_or(false) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    bail!("{CADDY_CONTAINER} started but its admin API never answered within 5s — it may be crash-looping (check the {CADDY_CONTAINER} container logs)")
}

/// What `*.<domain>` currently answers, measured against what the site needs.
///
/// The distinction a bare boolean could not make. `ws` used to ask only "loopback
/// or not-loopback", so a wildcard still pointing at a former offload server read
/// as **doesn't resolve yet** on a workspace that had been flipped back to local —
/// while the browser (and a vite `/api` proxy) happily kept reaching that server,
/// which answered with Caddy's "No stack is running here" page. It resolved fine;
/// it resolved to the wrong machine, and that is a different problem with
/// different advice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dns {
    /// The name doesn't resolve at all: NXDOMAIN, no resolver, or the lookup
    /// exceeded the probe deadline.
    Unresolved,
    /// It resolves to an address this site is served at.
    Correct,
    /// It resolves, but somewhere else. `got` is the first answer; `expected` is
    /// the address we wanted, when one was cheaply known (see [`expected_answer`]).
    Wrong { got: String, expected: Option<String> },
}

impl Dns {
    /// Whether a browser on this machine would find the site's routes.
    pub fn is_ok(&self) -> bool {
        matches!(self, Dns::Correct)
    }
}

/// Resolve the wildcard `*.<domain>` and judge the answer. The setup readiness
/// signal for the DNS bit, and the warning on `ws up`: routing works via
/// `curl --resolve` regardless, and wiring the wildcard into the resolver is a
/// privileged one-time step `ws setup` owns.
///
/// `expected` is the exact address the answer should carry when the caller knows
/// it cheaply. Without one, the check falls back to the right *kind* of address —
/// a **local** stack wants loopback, a **remote** stack wants anything else — which
/// still keeps a resolver that synthesizes an IP for every unknown name (ISP
/// NXDOMAIN hijacking, a captive portal) from reading as "resolved" locally.
///
/// The probe name often does *not* resolve (that's what fires the warning), so it
/// runs on a scratch thread with a short deadline: a slow or unreachable resolver
/// must not stall every `ws up` for seconds just to print a hint. A timeout counts
/// as [`Dns::Unresolved`] — the safe default (show the hint).
pub fn resolve_domain(site: &Site, domain: &str, expected: Option<&str>) -> Dns {
    // A label a real service is unlikely to shadow, so a hit means the wildcard
    // (not a coincidental host) resolves. The port is irrelevant to resolution.
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

/// Whether one resolved address is an acceptable answer for the wildcard.
///
/// A loopback expectation is satisfied by *any* loopback answer: `::1` and
/// `127.0.0.1` are the same machine, and "the machine you're on" is the whole of
/// what a local workspace asks for. A concrete non-loopback expectation is exact —
/// that's the point of threading it through.
fn answer_ok(got: &std::net::IpAddr, expected: Option<std::net::IpAddr>, want_loopback: bool) -> bool {
    match expected {
        Some(e) if e.is_loopback() => got.is_loopback(),
        Some(e) => *got == e,
        None => got.is_loopback() == want_loopback,
    }
}

/// Whether the singleton Caddy is currently running (for the setup readiness
/// probe). Externally-managed mode reports `true` — a BYO/test admin is "up" as
/// far as `ws` is concerned.
pub fn is_running(site: &Site, runtime: &str) -> bool {
    externally_managed() || container_running(site, runtime, CADDY_CONTAINER)
}

/// Fetch Caddy's internal root CA certificate (PEM) from the admin API, for the
/// setup step that trusts it in the system store. Caddy exposes it at
/// `/pki/ca/local` as `{ "root_certificate": "<PEM>", … }`.
pub fn root_ca_pem(site: &Site) -> Result<String> {
    let out = curl(site, &admin_args(site, "/pki/ca/local"))?;
    if !out.ok || !(200..300).contains(&out.code) {
        bail!("fetch Caddy root CA: HTTP {} {}", out.code, out.body.trim());
    }
    let v: serde_json::Value =
        serde_json::from_str(&out.body).context("parse /pki/ca/local response")?;
    v["root_certificate"]
        .as_str()
        .map(str::to_string)
        .context("no root_certificate in /pki/ca/local response")
}

/// Fetch Caddy's root CA and write it to a stable path **inside `ws`'s own config
/// dir** (`~/.config/ws/ws-caddy-root.crt`, user-owned 0600) — not a world- or
/// env-influenced temp dir — then return the path for the trust command. Keeping
/// it off `$TMPDIR`/`/tmp` closes the predictable-path clobber/symlink window
/// before a later `sudo` reads it.
pub fn write_root_ca(site: &Site) -> Result<PathBuf> {
    // Fetch from Caddy where it runs (`site`), but write to the HOST config dir —
    // the CA is trusted in the *client's* (host's) store regardless of where the
    // stack lives.
    let pem = root_ca_pem(site)?;
    let dir = config_dir();
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

/// Whether a container by this name is running at `site`.
fn container_running(site: &Site, runtime: &str, name: &str) -> bool {
    site.command(&[runtime, "container", "inspect", "-f", "{{.State.Running}}", name])
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true")
        .unwrap_or(false)
}

/// Where the pinned Caddy boot config lives (machine-global — one Caddy serves
/// every workspace on the box): the shared `~/.config/ws/` dir.
fn config_dir() -> PathBuf {
    crate::config::global_dir()
}

/// The absolute config dir at `site`, holding the pinned boot config
/// (`caddy.json`, bind-mounted into the Caddy container). Local: the host
/// `config_dir()`, created + canonicalized so the bind-mount source is absolute
/// (a relative `./.config/ws` from an unset HOME is rejected by the runtime).
/// Remote: `$HOME/.config/ws` on the server (already absolute — HOME was resolved
/// when the `Site` was built).
fn config_dir_at(site: &Site) -> PathBuf {
    match site {
        Site::Local => {
            let dir = config_dir();
            std::fs::create_dir_all(&dir).ok();
            std::fs::canonicalize(&dir).unwrap_or(dir)
        }
        Site::Remote { .. } => {
            PathBuf::from(site.home_path(".config/ws").expect("remote site carries a home"))
        }
    }
}


/// Write the pinned boot config (empty routes; everything else flows in over the
/// admin API) at `site` and return the absolute config dir (mounted into the
/// container). For a remote site the file is streamed to the server over SSH.
fn write_boot_config(site: &Site) -> Result<String> {
    let dir = config_dir_at(site).to_string_lossy().into_owned();
    let json = serde_json::to_string_pretty(&boot_config())? + "\n";
    site.write_file(&format!("{dir}/caddy.json"), &json)?;
    Ok(dir)
}

/// The Caddy boot config: admin API on a host-loopback-published TCP port (see
/// the module doc), one HTTPS server whose only route is the catch-all
/// stopped-stack page (worktree routes prepend ahead of it), and `tls internal`
/// (Caddy is the local CA).
///
/// **On-demand internal TLS.** The catch-all matches hostnames `ws` never
/// registered as a route (a stack that's down, or never came up), so Caddy has
/// no ahead-of-time cert for them — `on_demand` lets it mint one from its
/// internal CA at handshake time, turning what was a raw `tlsv1 internal error`
/// into the friendly page. Newer Caddy requires an `on_demand.permission` module
/// before it will issue on-demand — but that gate does **not** apply to the
/// `internal` issuer (local + free, no exhaustion risk). Verified against the
/// pinned image (`caddy:2`, v2.11.4): with only `on_demand: true` on an
/// `internal` policy it minted a cert for an unregistered host and served the
/// 503 page. So do *not* "fix" this by adding a permission endpoint.
///
/// **Seeds only a fresh box.** `run_caddy` passes `--resume`, so once an autosave
/// exists this boot config is ignored in favour of the persisted one — a change
/// here won't reach a box that already has a `ws-caddy` autosave (bring it down
/// / clear the config volume to re-seed).
fn boot_config() -> serde_json::Value {
    let port = https_port();
    serde_json::json!({
        // Bind the container's routable interface — a published port DNATs there,
        // not to the container's loopback. Protection is the host publish being
        // scoped to `127.0.0.1` (see `run_caddy`), which keeps the LAN out. We do
        // NOT set `enforce_origin`/`origins`: Caddy's origin check reads the
        // `Origin` header, which `curl` doesn't send, so it would reject `ws`'s own
        // requests. The residual (a `ws-net` tenant reaching `caddy:2019`) is the
        // documented local-dev tradeoff; a dedicated admin network is the fix.
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

/// The catch-all route (no host match ⇒ matches everything). It sits last in the
/// route list — worktree routes prepend ahead of it (see `route_up`) — so it only
/// fires for a hostname with no live stack, returning a self-explanatory
/// "run `ws up`" page (HTTP 503) instead of a bare TLS/connection error.
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

/// The route object Caddy stores: match the worktree hostname, reverse-proxy to
/// its upstream (`<project>-<service>-1:<port>` for a container, or
/// `host.containers.internal:<port>` for a host process).
fn route(id: &str, hostname: &str, upstream: &str, wildcard: bool) -> serde_json::Value {
    // `*.foo` matches exactly one label and does NOT match bare `foo`, so a
    // wildcard service lists both. On-demand internal TLS mints a cert per tenant
    // hostname at handshake, so no wildcard cert is needed.
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

/// Add (or replace) the route for a worktree service. Deletes any prior route
/// with the same `@id` first so a re-`up` with a changed upstream takes effect,
/// then **inserts the fresh one at index 0** — ahead of the catch-all
/// stopped-stack route, which must stay last so a specific host match wins.
/// Both are scoped operations, never `/load`. Order among worktree routes is
/// irrelevant (each has a distinct host match), so prepending is safe.
pub fn route_up(site: &Site, spec: &RouteSpec) -> Result<()> {
    delete_id(site, &spec.id); // ignore-absent
    let body = route(&spec.id, &spec.hostname, &spec.upstream, spec.wildcard).to_string();
    let mut args = vec![
        "-X".to_string(), "PUT".to_string(),
        "-H".to_string(), "Content-Type: application/json".to_string(),
        "-d".to_string(), body,
    ];
    args.extend(admin_args(site, &format!("/config/apps/http/servers/{SERVER}/routes/0")));
    check(&curl(site, &args)?, || format!("add route {}", spec.id))
}

/// Remove a worktree service's route. A missing route, or an unreachable Caddy,
/// is fine — teardown is best-effort (infallible by design).
pub fn route_down(site: &Site, id: &str) {
    delete_id(site, id);
}

fn delete_id(site: &Site, id: &str) {
    // Best-effort: a 404 (no such route yet) is expected on first up / double down.
    let mut args = vec!["-X".to_string(), "DELETE".to_string()];
    args.extend(admin_args(site, &format!("/id/{id}/")));
    let _ = curl(site, &args);
}

/// Run `curl` against the admin API at `site`, capturing the response and HTTP
/// status. Under an externally-managed Caddy (`WS_CADDY_ADMIN`, a BYO/test admin
/// URL) the request always runs **locally** regardless of `site` — that URL is a
/// host endpoint, not the server's admin port.
fn curl(site: &Site, args: &[String]) -> Result<CurlOut> {
    let local = Site::Local;
    let effective = if externally_managed() { &local } else { site };
    let mut argv: Vec<&str> = vec!["curl", "-sS", "-m", "10", "-w", "\n%{http_code}"];
    argv.extend(args.iter().map(String::as_str));
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

fn check(out: &CurlOut, what: impl FnOnce() -> String) -> Result<()> {
    if out.ok && (200..300).contains(&out.code) {
        return Ok(());
    }
    let detail = if !out.body.is_empty() { &out.body } else { &out.stderr };
    bail!("{}: HTTP {} {}", what(), out.code, detail.trim());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote_site() -> Site {
        Site::Remote { connection: "server".into(), home: "/home/ws".into() }
    }

    /// The whole point of the flag: docker resolves neither `host.containers.internal`
    /// nor its own `host.docker.internal` on Linux, so without it every `kind: host`
    /// route 502s. Podman defines the name itself and needs nothing.
    #[test]
    fn only_a_non_podman_runtime_gets_the_host_alias() {
        assert_eq!(host_alias_args("docker"), vec!["--add-host", "host.containers.internal:host-gateway"]);
        assert!(host_alias_args("podman").is_empty());
        // A runtime given by path is still podman.
        assert!(host_alias_args("/usr/bin/podman").is_empty());
        // Anything unrecognized gets the alias: podman tolerates the flag, so
        // adding it is the safe side of the guess.
        assert!(!host_alias_args("nerdctl").is_empty());
    }

    /// `--add-host` takes one `name:addr` token, so the alias is spelled out in
    /// `host_alias_args` rather than built from `HOST_ALIAS`. Pin them together —
    /// the flag and the upstream `inject::routes` dials must name the same host.
    #[test]
    fn the_host_alias_flag_matches_the_name_routes_dial() {
        let args = host_alias_args("docker");
        assert!(args[1].starts_with(&format!("{HOST_ALIAS}:")), "{args:?} must map {HOST_ALIAS}");
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
        let cfg = Some("100.97.195.105");
        assert_eq!(publish_address(&remote_site(), cfg), "100.97.195.105");
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
        assert_eq!(expected_answer("100.97.195.105").as_deref(), Some("100.97.195.105"));
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
        let srv: IpAddr = "100.97.195.105".parse().unwrap();
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
            got: "100.97.195.105".to_string(),
            expected: Some("127.0.0.1".to_string()),
        };
        let hint = resolve_hint(&Site::Local, "safetyamp.test", &dns).expect("a wrong answer hints");
        assert!(hint.contains("resolves to 100.97.195.105"), "{hint}");
        assert!(hint.contains("expected 127.0.0.1"), "{hint}");
        assert!(!hint.contains("doesn't resolve"), "wrong answer is not a missing one: {hint}");

        // Not resolving at all keeps its own, distinct advice.
        let missing = resolve_hint(&Site::Local, "safetyamp.test", &Dns::Unresolved).unwrap();
        assert!(missing.contains("doesn't resolve yet"), "{missing}");
        assert!(missing.contains("curl --resolve"), "{missing}");

        // And a correct wildcard says nothing at all.
        assert_eq!(resolve_hint(&Site::Local, "safetyamp.test", &Dns::Correct), None);
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
    fn admin_args_hit_the_loopback_admin_port() {
        // Reads `admin_port()` twice (inside `admin_args` and in the expectation),
        // so it must hold the env guard — else a concurrent `WS_CADDY_ADMIN_PORT`
        // mutation between the two reads makes them disagree.
        let _g = env_guard();
        let args = admin_args(&Site::Local, "/config/");
        assert_eq!(args, vec![format!("http://127.0.0.1:{}/config/", admin_port())]);
    }
}
