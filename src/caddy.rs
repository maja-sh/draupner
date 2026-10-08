//! Containerized Caddy on the shared `wsx-net`, driven by its admin API.
//!
//! Caddy runs as a container (rootless host Caddy can't resolve container DNS)
//! and dials worktree containers by `<project>-<service>-1`; host processes via
//! `host.containers.internal`. Route CRUD is scoped by `@id` — never `/load` —
//! so parallel `wsx` processes with distinct ids don't race. The admin API is
//! published on a host-loopback TCP port (a unix socket can't cross the macOS
//! podman-machine VM boundary). `WSX_CADDY_ADMIN` treats Caddy + `wsx-net` as
//! externally managed (BYO / test stub).

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

use crate::remote::Site;

pub const WSX_NET: &str = "wsx-net";
pub const CADDY_CONTAINER: &str = "ws-caddy";
const CADDY_IMAGE: &str = "docker.io/library/caddy:2";
const SERVER: &str = "wsx";
/// Non-privileged so Caddy binds with no setup; `:443` needs the sysctl.
pub const DEFAULT_HTTPS_PORT: u16 = 8443;
const SOCK_MOUNT: &str = "/sock";
/// Configurable: a host already running its own Caddy holds this exact port.
const DEFAULT_ADMIN_PORT: u16 = 2019;
/// CA root + leaf certs; persisted so the root stays trusted across recreation.
const DATA_VOLUME: &str = "ws-caddy-data";
/// `--resume` autosave; persisted so a recreated `ws-caddy` reloads every route.
const CONFIG_VOLUME: &str = "ws-caddy-config";
/// Distinct from any `route_id` so worktree route CRUD never touches it.
const STOPPED_STACK_ID: &str = "ws-stopped-stack";
/// Second listener carrying only shared tasks' routes; its host publish is the LAN-facing one.
const SHARE_SERVER: &str = "wsx-share";
const UNSHARED_ID: &str = "ws-share-unshared";
/// Route ids all start `ws-`, so this prefix can't collide with one.
const SHARE_ID_PREFIX: &str = "share-";
pub const HOST_ALIAS: &str = "host.containers.internal";

fn admin_args(_site: &Site, path: &str) -> Vec<String> {
    // `_site` unused: for remote, `curl` runs on the server (see `curl`).
    if let Ok(url) = std::env::var("WSX_CADDY_ADMIN") {
        vec![format!("{}{}", url.trim_end_matches('/'), path)]
    } else {
        vec![format!("http://127.0.0.1:{}{path}", admin_port())]
    }
}

pub fn externally_managed() -> bool {
    std::env::var_os("WSX_CADDY_ADMIN").is_some()
}

pub fn https_port() -> u16 {
    // A `0` (truncated file / bad env) would publish port 0 — fall through instead.
    if let Some(p) = std::env::var("WSX_HTTPS_PORT").ok().and_then(|v| v.trim().parse().ok()) {
        if p != 0 {
            return p;
        }
    }
    config_dir()
        .ok()
        .and_then(|d| std::fs::read_to_string(d.join("https_port")).ok())
        .and_then(|s| s.trim().parse().ok())
        .filter(|&p| p != 0)
        .unwrap_or(DEFAULT_HTTPS_PORT)
}

pub fn port_suffix(port: u16) -> String {
    if port == 443 {
        String::new()
    } else {
        format!(":{port}")
    }
}

/// Container-side only: the host publishes it on the same port number as the private listener, so a URL is identical from either side.
fn share_inner_port() -> u16 {
    let p = https_port();
    if p == u16::MAX { p - 1 } else { p + 1 }
}

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

/// IPv6 hosts must be bracketed or the runtime rejects `-p` outright.
fn publish_spec(addr: &str, host_port: u16, container_port: u16) -> String {
    match addr.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(a)) => format!("[{a}]:{host_port}:{container_port}"),
        _ => format!("{addr}:{host_port}:{container_port}"),
    }
}

/// `None` = "can't tell", never a mismatch — callers rely on this contract.
fn published_host(site: &Site, runtime: &str, port: u16) -> Option<String> {
    let out = site
        .command(&[runtime, "port", CADDY_CONTAINER, &port.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout).lines().next()?.trim().to_string();
    // Split at the last colon to keep IPv6 (`[::1]:8443`) intact.
    let host = line.rsplit_once(':')?.0.to_string();
    (!host.is_empty()).then(|| host.trim_matches(['[', ']']).to_string())
}

/// Present ⇒ publish the share listener; non-empty contents pin its address.
fn share_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("share"))
}

pub fn want_share(pin: Option<&str>) -> Result<()> {
    let path = share_file()?;
    // Re-sharing without `--address` keeps an earlier pin.
    if pin.is_none() && path.exists() {
        return Ok(());
    }
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    crate::config::write_atomic(&path, format!("{}\n", pin.unwrap_or("")).as_bytes())
        .context("persist share")
}

pub fn drop_share() {
    if let Ok(p) = share_file() {
        let _ = std::fs::remove_file(p);
    }
}

/// Only a local Caddy shares: an offloaded one already publishes on the server's interfaces.
pub fn share_address(site: &Site) -> Option<String> {
    if site.is_remote() {
        return None;
    }
    let pinned = std::fs::read_to_string(share_file().ok()?).ok()?;
    match pinned.trim() {
        "" => lan_address(),
        a => Some(a.to_string()),
    }
}

fn lan_address() -> Option<String> {
    // A route lookup only; nothing is sent to 1.1.1.1.
    if let Some(a) = stdout_of(&["ip", "-4", "route", "get", "1.1.1.1"]).and_then(|o| parse_ip_route_src(&o)) {
        return Some(a);
    }
    let iface = stdout_of(&["route", "-n", "get", "default"]).and_then(|o| parse_route_interface(&o))?;
    stdout_of(&["ipconfig", "getifaddr", &iface])
        .map(|o| o.trim().to_string())
        .filter(|a| a.parse::<std::net::IpAddr>().is_ok())
}

fn stdout_of(argv: &[&str]) -> Option<String> {
    let out = std::process::Command::new(argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Linux `ip route get`: `1.1.1.1 via 192.168.8.1 dev enp1s0 src 192.168.8.114 uid 1000`.
fn parse_ip_route_src(out: &str) -> Option<String> {
    let mut words = out.split_whitespace();
    words.find(|w| *w == "src")?;
    words.next().filter(|a| a.parse::<std::net::IpAddr>().is_ok()).map(str::to_string)
}

/// macOS `route -n get default`: an `interface: en0` line.
fn parse_route_interface(out: &str) -> Option<String> {
    out.lines()
        .find_map(|l| l.trim().strip_prefix("interface:"))
        .map(|i| i.trim().to_string())
        .filter(|i| !i.is_empty() && i.chars().all(|c| c.is_ascii_alphanumeric()))
}

pub fn set_https_port(port: u16) -> Result<()> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    crate::config::write_atomic(&dir.join("https_port"), format!("{port}\n").as_bytes())
        .context("persist https_port")
}

pub fn admin_port() -> u16 {
    if let Some(p) = std::env::var("WSX_CADDY_ADMIN_PORT").ok().and_then(|v| v.trim().parse().ok()) {
        if p != 0 {
            return p;
        }
    }
    config_dir()
        .ok()
        .and_then(|d| std::fs::read_to_string(d.join("admin_port")).ok())
        .and_then(|s| s.trim().parse().ok())
        .filter(|&p| p != 0)
        .unwrap_or(DEFAULT_ADMIN_PORT)
}

pub fn set_admin_port(port: u16) -> Result<()> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    crate::config::write_atomic(&dir.join("admin_port"), format!("{port}\n").as_bytes())
        .context("persist admin_port")
}

/// Repo included so same-named services in two repos don't collide on one `@id`.
///
/// Each segment is length-prefixed (`<len>_<value>`) so `(feat-web, api, x)`
/// and `(feat, web-api, x)` can't collapse to the same id via the `-` split —
/// bare concatenation had those two tuples producing `wsx-feat-web-api-x` and
/// the second worktree silently clobbered the first's route.
pub fn route_id(branch_sanitized: &str, repo: &str, service: &str) -> String {
    let a = id_part(branch_sanitized);
    let b = id_part(repo);
    let c = id_part(service);
    format!("ws-{}_{a}-{}_{b}-{}_{c}", a.len(), b.len(), c.len())
}

/// The id becomes one segment of the admin API path, and `.wsx.json` (attacker-controlled) supplies `repo`/`service`; a slash or `..` in either would let curl's dot-segment normalisation retarget the request.
fn id_part(s: &str) -> String {
    s.replace(|c: char| !(c.is_ascii_alphanumeric() || c == '_'), "-")
}

pub struct RouteSpec {
    pub id: String,
    pub hostname: String,
    pub upstream: String,
    pub wildcard: bool,
}

fn runtime_available(site: &Site, runtime: &str) -> Result<()> {
    // Remote binaries live on the server; the client's PATH says nothing.
    if site.is_remote() {
        return Ok(());
    }
    crate::config::ensure_available(runtime)
}

pub fn ensure_net(site: &Site, runtime: &str) -> Result<()> {
    if externally_managed() {
        return Ok(());
    }
    runtime_available(site, runtime)?;
    let out = site
        .command(&[runtime, "network", "create", WSX_NET])
        .output()
        .with_context(|| format!("spawn `{runtime} network create`"))?;
    if out.status.success() {
        return Ok(());
    }
    // Case-insensitive so both podman ("...already exists") and docker phrasings match.
    let err = String::from_utf8_lossy(&out.stderr);
    let lower = err.to_lowercase();
    if lower.contains("already") || lower.contains("exists") {
        return Ok(());
    }
    bail!("create {WSX_NET}: {}", err.trim());
}

/// Race-safe: never removes a running container (peer's live routes); only stale
/// stopped containers are retried.
pub fn ensure_running(site: &Site, runtime: &str, publish_addr: &str) -> Result<()> {
    if externally_managed() {
        return Ok(());
    }
    runtime_available(site, runtime)?;
    let share = share_address(site);
    if container_running(site, runtime, CADDY_CONTAINER) {
        // Left alone unless port or address moved; a `-p` binding is invisible
        // to Caddy so address changes can't be inferred from its config.
        match served_port(site) {
            Some(p) if p == https_port() => {
                match publish_drift(site, runtime, p, publish_addr, share.as_deref()) {
                    Some(why) => {
                        crate::ui::note(&format!(
                            "re-creating {CADDY_CONTAINER} to {why} — routes are preserved"
                        ));
                        // Cheaper than `reseed`: `--resume` reloads routes from the retained volume.
                        let _ = site.command(&[runtime, "rm", "-f", CADDY_CONTAINER]).output();
                    }
                    None => return Ok(()),
                }
            }
            // Predates the TCP-admin switch or wedged; `--resume` would restore the old admin.
            None => {
                crate::ui::note(&format!(
                    "{CADDY_CONTAINER} is running but its admin is unreachable — \
                     re-seeding it (routes reinstall on `wsx up`)"
                ));
                reseed(site, runtime);
            }
            // Don't tear down a working Caddy for a port that won't bind.
            Some(_) if !port_bindable(site, https_port()) => return Ok(()),
            Some(_) => {
                crate::ui::note(&format!(
                    "re-seeding {CADDY_CONTAINER} for the :{} move — routes reinstall on `wsx up`",
                    https_port()
                ));
                reseed(site, runtime);
            }
        }
    }
    ensure_net(site, runtime)?;

    // Otherwise a raw `rootlessport ... address already in use` + half-created container.
    preflight_ports(site)?;

    let dir = write_boot_config(site)?;
    let share = share.as_deref();
    if run_caddy(site, runtime, &dir, publish_addr, share).is_none() {
        return wait_admin_ready(site);
    }
    if container_running(site, runtime, CADDY_CONTAINER) {
        return wait_admin_ready(site);
    }
    let _ = site.command(&[runtime, "rm", "-f", CADDY_CONTAINER]).output();
    let reason = run_caddy(site, runtime, &dir, publish_addr, share);
    if reason.is_none() || container_running(site, runtime, CADDY_CONTAINER) {
        return wait_admin_ready(site);
    }
    // Container never started, so `podman logs` is empty — surface the reason.
    bail!(
        "could not start the {CADDY_CONTAINER} container: {}",
        reason.unwrap_or_else(|| "unknown error".to_string())
    )
}

fn publish_drift(
    site: &Site,
    runtime: &str,
    port: u16,
    publish_addr: &str,
    share: Option<&str>,
) -> Option<String> {
    if let Some(h) = published_host(site, runtime, port).filter(|h| h != publish_addr) {
        return Some(format!("publish on {publish_addr} (currently {h})"));
    }
    match (share, published_host(site, runtime, share_inner_port()).as_deref()) {
        (Some(want), Some(have)) if want == have => None,
        (Some(want), _) => Some(format!("share on {want}")),
        (None, Some(have)) => Some(format!("stop sharing on {have}")),
        (None, None) => None,
    }
}

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
                 existing Caddy) holds it. Move wsx's Caddy off it with `wsx setup {flag} <port>`."
            );
        }
    }
    Ok(())
}

/// `None` = "don't touch" — callers rely on this contract.
fn served_port(site: &Site) -> Option<u16> {
    let out = curl(site, &admin_args(site, &format!("/config/apps/http/servers/{SERVER}/listen"))).ok()?;
    if !out.ok || !(200..300).contains(&out.code) {
        return None;
    }
    parse_listen_port(&out.body)
}

fn parse_listen_port(body: &str) -> Option<u16> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get(0)?.as_str()?.rsplit(':').next()?.parse().ok()
}

/// Drops the config volume (which pins the port via `--resume`); keeps `ws-caddy-data` so the CA stays trusted.
fn reseed(site: &Site, runtime: &str) {
    let _ = site.command(&[runtime, "rm", "-f", CADDY_CONTAINER]).output();
    let _ = site.command(&[runtime, "volume", "rm", CONFIG_VOLUME]).output();
}

/// Unreadable sysctl reads as not bindable — never tear down a working Caddy for an unverifiable port.
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

/// Fatal on infra failure: the override drops HTTP host-ports, so "up" without Caddy would be a lie.
pub fn ensure_routing(site: &Site, runtime: &str, domain: &str, publish_addr: &str) -> Result<Dns> {
    ensure_net(site, runtime).with_context(|| format!("routing needs the `{WSX_NET}` network"))?;
    ensure_running(site, runtime, publish_addr)
        .context("caddy must be running to route HTTP services")?;
    Ok(resolve_domain(site, domain, expected_answer(publish_addr).as_deref()))
}

/// `None` for bind-any: deriving the client's address costs an `ssh -G`.
pub fn expected_answer(publish_addr: &str) -> Option<String> {
    publish_addr
        .trim()
        .parse::<std::net::IpAddr>()
        .ok()
        .filter(|ip| !ip.is_unspecified())
        .map(|ip| ip.to_string())
}

/// Distinct advice for "no answer" vs "wrong answer" — conflating them made a stale offload address read as "doesn't resolve yet" after flipping back to local.
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
             Re-run `wsx setup` to re-point it."
        )),
        Dns::Wrong { got, expected: None } => Some(format!(
            "`*.{domain}` resolves to {got}, but this workspace's stacks run on the server — \
             point the wildcard at an address the server is reachable at \
             (see `server-setup/<distro>.sh --address`)"
        )),
    }
}

/// Docker on Linux defines neither [`HOST_ALIAS`] nor `host.docker.internal`, so
/// `kind: host` routes 502 without `--add-host`. Keyed on "not podman" so unknown
/// runtimes get it too (podman tolerates the flag).
fn host_alias_args(runtime: &str) -> Vec<&'static str> {
    // `--add-host` takes one token, no const concat — spelled out.
    const ALIAS_TO_GATEWAY: &str = "host.containers.internal:host-gateway";

    if runtime.ends_with("podman") {
        return Vec::new();
    }
    vec!["--add-host", ALIAS_TO_GATEWAY]
}

fn run_caddy(
    site: &Site,
    runtime: &str,
    dir: &str,
    publish_addr: &str,
    share: Option<&str>,
) -> Option<String> {
    let port = https_port();
    let publish = publish_spec(publish_addr, port, port);
    let share_publish = share.map(|a| publish_spec(a, port, share_inner_port()));
    let admin = admin_port();
    // Loopback-only publish; container listens on 2019 inside.
    let admin_publish = format!("127.0.0.1:{admin}:{DEFAULT_ADMIN_PORT}");
    let sock_mount = format!("{dir}:{SOCK_MOUNT}:Z");
    let data_vol = format!("{DATA_VOLUME}:/data");
    let config_vol = format!("{CONFIG_VOLUME}:/config");
    let config_json = format!("{SOCK_MOUNT}/caddy.json");
    let mut args = vec![
        runtime, "run", "-d", "--name", CADDY_CONTAINER, "--network", WSX_NET, "--restart", "always",
    ];
    args.extend(host_alias_args(runtime));
    args.extend(["-p", &publish]);
    if let Some(s) = share_publish.as_deref() {
        args.extend(["-p", s]);
    }
    args.extend([
        "-p", &admin_publish,
        "-v", &sock_mount,
        "-v", &data_vol,
        "-v", &config_vol,
        CADDY_IMAGE,
        "caddy", "run", "--config", &config_json, "--resume",
    ]);
    let out = site.command(&args).output();
    // Return (not print) so a first attempt that recovers on retry stays quiet.
    match out {
        Ok(o) if o.status.success() => None,
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            Some(err.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("failed").to_string())
        }
        Err(e) => Some(e.to_string()),
    }
}

/// Caddy's listener isn't up when `run -d` returns; without this an immediately-following `route_up` could hit a not-yet-listening socket.
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

/// A bare boolean can't tell "no answer" from "wrong answer" — different advice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dns {
    Unresolved,
    Correct,
    Wrong { got: String, expected: Option<String> },
}

impl Dns {
    pub fn is_ok(&self) -> bool {
        matches!(self, Dns::Correct)
    }
}

/// Scratch thread with a short deadline so a slow resolver doesn't stall `wsx up`.
/// Without `expected`, falls back to loopback-vs-not so NXDOMAIN-hijacking resolvers don't read as "resolved".
pub fn resolve_domain(site: &Site, domain: &str, expected: Option<&str>) -> Dns {
    // A label unlikely to be shadowed by a real service, so a hit means the wildcard resolves.
    let probe = format!("ws-dns-probe.{domain}:{}", https_port());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::net::ToSocketAddrs;
        let addrs: Vec<std::net::IpAddr> = probe
            .to_socket_addrs()
            .map(|a| a.map(|s| s.ip()).collect())
            .unwrap_or_default();
        let _ = tx.send(addrs); // receiver may have already timed out
    });
    let answers = rx.recv_timeout(std::time::Duration::from_millis(300)).unwrap_or_default();
    let Some(first) = answers.first() else { return Dns::Unresolved };

    let want = expected.and_then(|a| a.trim().parse::<std::net::IpAddr>().ok());
    if answers.iter().any(|a| answer_ok(a, want, !site.is_remote())) {
        return Dns::Correct;
    }
    Dns::Wrong { got: first.to_string(), expected: expected.map(str::to_string) }
}

/// Loopback expectation accepts any loopback answer (`::1` == `127.0.0.1` here); a concrete non-loopback expectation is exact.
fn answer_ok(got: &std::net::IpAddr, expected: Option<std::net::IpAddr>, want_loopback: bool) -> bool {
    match expected {
        Some(e) if e.is_loopback() => got.is_loopback(),
        Some(e) => *got == e,
        None => got.is_loopback() == want_loopback,
    }
}

pub fn is_running(site: &Site, runtime: &str) -> bool {
    externally_managed() || container_running(site, runtime, CADDY_CONTAINER)
}

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

/// Written to `~/.config/wsx/` (0600), not `$TMPDIR`, to close the predictable-path clobber/symlink window before `sudo` reads it.
pub fn write_root_ca(site: &Site) -> Result<PathBuf> {
    // Fetched at `site` where Caddy runs, written to the HOST dir — CA is trusted in the client's store.
    let pem = root_ca_pem(site)?;
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join("wsx-caddy-root.crt");
    crate::config::write_atomic(&path, pem.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(path)
}

fn container_running(site: &Site, runtime: &str, name: &str) -> bool {
    site.command(&[runtime, "container", "inspect", "-f", "{{.State.Running}}", name])
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true")
        .unwrap_or(false)
}

fn config_dir() -> Result<PathBuf> {
    crate::config::global_dir()
}

/// Canonicalized locally so the bind-mount source is absolute (relative paths are runtime-rejected).
fn config_dir_at(site: &Site) -> Result<PathBuf> {
    match site {
        Site::Local => {
            let dir = config_dir()?;
            std::fs::create_dir_all(&dir).ok();
            Ok(std::fs::canonicalize(&dir).unwrap_or(dir))
        }
        Site::Remote { .. } => {
            Ok(PathBuf::from(site.home_path(".config/wsx").expect("remote site carries a home")))
        }
    }
}


fn write_boot_config(site: &Site) -> Result<String> {
    let dir = config_dir_at(site)?.to_string_lossy().into_owned();
    let json = serde_json::to_string_pretty(&boot_config())? + "\n";
    site.write_file(&format!("{dir}/caddy.json"), &json)?;
    Ok(dir)
}

/// `on_demand: true` on `internal` mints certs at handshake for unregistered
/// hostnames; the `on_demand.permission` gate does NOT apply to `internal`
/// (verified caddy:2 v2.11.4) — do NOT add a permission endpoint. Seeds only a
/// fresh box: `--resume` ignores this once an autosave exists.
fn boot_config() -> serde_json::Value {
    let port = https_port();
    serde_json::json!({
        // Bind the routable interface (published ports DNAT there, not to
        // loopback); protection is the loopback-scoped host publish. `origins`
        // can't be used — Caddy's origin check reads `Origin` which `curl` omits.
        "admin": { "listen": format!("0.0.0.0:{DEFAULT_ADMIN_PORT}") },
        "apps": {
            "http": {
                "https_port": port,
                "servers": {
                    SERVER: { "listen": [format!(":{port}")], "routes": [ stopped_stack_route() ] },
                    SHARE_SERVER: share_server()
                }
            },
            "tls": { "automation": {
                "policies": [ { "issuers": [ { "module": "internal" } ], "on_demand": true } ]
            } }
        }
    })
}

fn stopped_stack_route() -> serde_json::Value {
    let body = "<!doctype html><meta charset=utf-8>\
        <title>wsx — no stack here</title>\
        <style>body{font-family:system-ui,sans-serif;max-width:34rem;margin:12vh auto;padding:0 1.5rem;line-height:1.5}\
        code{background:#eee;padding:.15em .4em;border-radius:.3em}</style>\
        <h1>No stack is running here</h1>\
        <p>Nothing is routed for this hostname yet. Bring its worktree up with:</p>\
        <p><code>wsx up &lt;branch&gt;</code></p>";
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

fn share_server() -> serde_json::Value {
    serde_json::json!({
        "listen": [format!(":{}", share_inner_port())],
        // Off `https_port`, Caddy only enables TLS for host-matched routes, so with none shared it would answer plain HTTP.
        "tls_connection_policies": [ {} ],
        "routes": [ {
            "@id": UNSHARED_ID,
            "handle": [ {
                "handler": "static_response",
                "status_code": 503,
                "headers": { "Content-Type": [ "text/plain; charset=utf-8" ] },
                "body": "Nothing is shared at this address right now.\n"
            } ]
        } ]
    })
}

/// A Caddy seeded before sharing existed resumes without the share server.
pub fn ensure_share_server(site: &Site) -> Result<()> {
    let path = format!("/config/apps/http/servers/{SHARE_SERVER}");
    let out = curl(site, &admin_args(site, &path))?;
    if out.body.trim() != "null" {
        return Ok(());
    }
    let mut args = vec![
        "-X".to_string(), "PUT".to_string(),
        "-H".to_string(), "Content-Type: application/json".to_string(),
        "-d".to_string(), share_server().to_string(),
    ];
    args.extend(admin_args(site, &path));
    check(&curl(site, &args)?, || format!("add the {SHARE_SERVER} server"))
}

fn share_id(id: &str) -> String {
    format!("{SHARE_ID_PREFIX}{id}")
}

fn route(id: &str, hostname: &str, upstream: &str, wildcard: bool) -> serde_json::Value {
    // `*.foo` matches one label, NOT bare `foo` — a wildcard service lists both.
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

pub fn route_up(site: &Site, spec: &RouteSpec) -> Result<()> {
    put_route(site, SERVER, &spec.id, spec)
}

pub fn share_route_up(site: &Site, spec: &RouteSpec) -> Result<()> {
    put_route(site, SHARE_SERVER, &share_id(&spec.id), spec)
}

/// Prepends at index 0 — ahead of the catch-all which must stay last. Scoped ops, never `/load`.
fn put_route(site: &Site, server: &str, id: &str, spec: &RouteSpec) -> Result<()> {
    delete_id(site, id);
    let body = route(id, &spec.hostname, &spec.upstream, spec.wildcard).to_string();
    let mut args = vec![
        "-X".to_string(), "PUT".to_string(),
        "-H".to_string(), "Content-Type: application/json".to_string(),
        "-d".to_string(), body,
    ];
    args.extend(admin_args(site, &format!("/config/apps/http/servers/{server}/routes/0")));
    check(&curl(site, &args)?, || format!("add route {id}"))
}

/// Best-effort — teardown is infallible by design.
pub fn route_down(site: &Site, id: &str) {
    delete_id(site, id);
    delete_id(site, &share_id(id));
}

pub fn share_route_down(site: &Site, id: &str) {
    delete_id(site, &share_id(id));
}

/// Whether the private route is installed, i.e. the task is up and reachable here.
pub fn route_live(site: &Site, id: &str) -> bool {
    curl(site, &admin_args(site, &format!("/id/{id}/")))
        .map(|o| o.ok && (200..300).contains(&o.code))
        .unwrap_or(false)
}

/// Across every workspace on this machine, since they share the one Caddy; `None` = can't tell.
pub fn shared_route_count(site: &Site) -> Option<usize> {
    let out = curl(site, &admin_args(site, &format!("/config/apps/http/servers/{SHARE_SERVER}/routes"))).ok()?;
    if !out.ok || !(200..300).contains(&out.code) {
        return None;
    }
    count_shared(&out.body)
}

fn count_shared(body: &str) -> Option<usize> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    if v.is_null() {
        return Some(0);
    }
    Some(
        v.as_array()?
            .iter()
            .filter(|r| r["@id"].as_str().is_some_and(|id| id.starts_with(SHARE_ID_PREFIX)))
            .count(),
    )
}

fn cert_id(domain: &str) -> String {
    format!("ws-cert-{}", id_part(domain))
}

/// Replaces any copy loaded earlier, so renewed files take effect on the next push.
pub fn load_cert(site: &Site, domain: &str, cert_pem: &str, key_pem: &str) -> Result<()> {
    let id = cert_id(domain);
    delete_id(site, &id);
    let pair = serde_json::json!({ "@id": id, "certificate": cert_pem, "key": key_pem });
    let path = "/config/apps/tls/certificates/load_pem";
    // POST appends to an existing array; only PUT creates the missing parents.
    let existing = curl(site, &admin_args(site, path))?;
    let appendable = existing.ok
        && serde_json::from_str::<serde_json::Value>(&existing.body).is_ok_and(|v| v.is_array());
    let (method, body) = if appendable { ("POST", pair) } else { ("PUT", serde_json::json!([pair])) };
    let mut args = vec![
        "-X".to_string(), method.to_string(),
        "-H".to_string(), "Content-Type: application/json".to_string(),
        "--data-binary".to_string(), "@-".to_string(),
    ];
    args.extend(admin_args(site, path));
    check(&curl_with_stdin(site, &args, Some(&body.to_string()))?, || format!("load the certificate for {domain}"))
}

pub fn unload_cert(site: &Site, domain: &str) {
    delete_id(site, &cert_id(domain));
}

fn delete_id(site: &Site, id: &str) {
    let mut args = vec!["-X".to_string(), "DELETE".to_string()];
    args.extend(admin_args(site, &format!("/id/{id}/")));
    let _ = curl(site, &args);
}

/// Under `WSX_CADDY_ADMIN` the request always runs locally regardless of `site` — that URL is a host endpoint, not the server's admin port.
fn curl(site: &Site, args: &[String]) -> Result<CurlOut> {
    curl_with_stdin(site, args, None)
}

/// A private key travels on stdin, never argv, where any local user's `ps` would see it.
fn curl_with_stdin(site: &Site, args: &[String], stdin: Option<&str>) -> Result<CurlOut> {
    use std::io::Write;
    use std::process::Stdio;

    let local = Site::Local;
    let effective = if externally_managed() { &local } else { site };
    let mut argv: Vec<&str> = vec!["curl", "-sS", "-m", "10", "-w", "\n%{http_code}"];
    argv.extend(args.iter().map(String::as_str));
    let mut child = effective
        .command(&argv)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn curl for Caddy admin API")?;
    if let Some(body) = stdin {
        child.stdin.take().expect("piped stdin").write_all(body.as_bytes()).context("stream to curl")?;
    }
    let out = child.wait_with_output().context("wait for curl")?;
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

    #[test]
    fn only_a_non_podman_runtime_gets_the_host_alias() {
        assert_eq!(host_alias_args("docker"), vec!["--add-host", "host.containers.internal:host-gateway"]);
        assert!(host_alias_args("podman").is_empty());
        assert!(host_alias_args("/usr/bin/podman").is_empty());
        assert!(!host_alias_args("nerdctl").is_empty());
    }

    /// Pins the flag and the upstream `inject::routes` dials to the same host.
    #[test]
    fn the_host_alias_flag_matches_the_name_routes_dial() {
        let args = host_alias_args("docker");
        assert!(args[1].starts_with(&format!("{HOST_ALIAS}:")), "{args:?} must map {HOST_ALIAS}");
    }

    #[test]
    fn publish_defaults_keep_local_private_and_make_remote_reachable() {
        assert_eq!(publish_address(&Site::Local, None), "127.0.0.1");
        assert_eq!(publish_address(&remote_site(), None), "0.0.0.0");
    }

    #[test]
    fn a_configured_address_narrows_only_the_remote_site() {
        let cfg = Some("100.64.1.3");
        assert_eq!(publish_address(&remote_site(), cfg), "100.64.1.3");
        // Local ignores it: `wsx setup`'s CA probe and `resolve_*` assume loopback.
        assert_eq!(publish_address(&Site::Local, cfg), "127.0.0.1");
    }

    #[test]
    fn blank_publish_address_falls_back_to_the_default() {
        assert_eq!(publish_address(&remote_site(), Some("")), "0.0.0.0");
        assert_eq!(publish_address(&remote_site(), Some("   ")), "0.0.0.0");
    }

    #[test]
    fn publish_spec_brackets_ipv6_only() {
        assert_eq!(publish_spec("127.0.0.1", 8443, 8443), "127.0.0.1:8443:8443");
        assert_eq!(publish_spec("0.0.0.0", 8443, 8443), "0.0.0.0:8443:8443");
        assert_eq!(publish_spec("::1", 8443, 8443), "[::1]:8443:8443");
        assert_eq!(publish_spec("fd7a:115c::1", 443, 444), "[fd7a:115c::1]:443:444");
    }

    #[test]
    fn route_id_is_stable_and_namespaced() {
        assert_eq!(route_id("feat-foo", "api", "web"), "ws-8_feat-foo-3_api-3_web");
        assert_ne!(route_id("feat-foo", "api", "web"), route_id("feat-foo", "ui", "web"));
        // Ordinary names survive their id_part encoding, including the underscores
        // and digits compose service names routinely carry.
        assert_eq!(route_id("release-1.2", "web_api", "app2"), "ws-11_release-1-2-7_web_api-4_app2");
    }

    #[test]
    fn route_id_does_not_collide_across_dash_split() {
        // Bare concatenation collapsed these two tuples onto the same id
        // (`wsx-feat-web-api-x`), and Caddy's `@id`-keyed CRUD meant the second
        // worktree silently overwrote the first's route in the admin config.
        // Length-prefixed segments are provably unique per tuple.
        let a = route_id("feat-web", "api", "x");
        let b = route_id("feat", "web-api", "x");
        assert_ne!(a, b, "distinct tuples must produce distinct ids: {a} vs {b}");

        // Same collision family with underscores instead of dashes.
        let c = route_id("feat_web", "api", "x");
        let d = route_id("feat", "web_api", "x");
        assert_ne!(c, d, "distinct tuples must produce distinct ids: {c} vs {d}");
    }

    #[test]
    fn route_id_cannot_escape_its_admin_path_segment() {
        // A cloned `.wsx.json` names a service `/../../config`: interpolated raw,
        // `/id/<id>/` becomes a path curl normalizes down to `/config/`, so the
        // best-effort route delete would wipe the whole running Caddy config.
        let id = route_id("feat-foo", "api", "/../../config");
        assert!(!id.contains('/'), "id must stay one path segment: {id}");
        assert!(!id.contains(".."), "id must carry no dot segments: {id}");
        // Same for a hostile repo name, the other `.wsx.json`-controlled part.
        let id = route_id("feat-foo", "../../..", "web");
        assert!(!id.contains('/') && !id.contains(".."), "{id}");
    }

    #[test]
    fn route_json_matches_host_and_proxies_upstream() {
        let r = route("wsx-feat-foo-web", "feat-foo.web.myapp.test", "feat-foo-api-web-1:80", false);
        assert_eq!(r["@id"], "wsx-feat-foo-web");
        assert_eq!(r["match"][0]["host"][0], "feat-foo.web.myapp.test");
        assert_eq!(r["match"][0]["host"].as_array().unwrap().len(), 1);
        assert_eq!(r["handle"][0]["handler"], "reverse_proxy");
        assert_eq!(r["handle"][0]["upstreams"][0]["dial"], "feat-foo-api-web-1:80");
    }

    #[test]
    fn wildcard_route_matches_the_base_and_every_tenant_subdomain() {
        let r = route("wsx-feat-foo-app", "feat-foo.app.myapp.test", "host.containers.internal:20001", true);
        let hosts: Vec<&str> =
            r["match"][0]["host"].as_array().unwrap().iter().map(|h| h.as_str().unwrap()).collect();
        // `*.foo` alone would NOT match bare `foo`, so both must be present.
        assert!(hosts.contains(&"feat-foo.app.myapp.test"), "missing base host: {hosts:?}");
        assert!(hosts.contains(&"*.feat-foo.app.myapp.test"), "missing tenant wildcard: {hosts:?}");
        assert_eq!(hosts.len(), 2);
    }

    #[test]
    fn boot_config_has_on_demand_internal_tls_and_the_catch_all() {
        // Hold the port-env lock so a concurrent `WSX_HTTPS_PORT` mutation can't
        // change it between the two reads.
        let _g = env_guard();
        let port = https_port();
        let c = boot_config();
        let policy = &c["apps"]["tls"]["automation"]["policies"][0];
        assert_eq!(policy["issuers"][0]["module"], "internal");
        assert_eq!(policy["on_demand"], true);
        assert_eq!(c["apps"]["http"]["servers"][SERVER]["listen"][0], format!(":{port}"));

        let routes = c["apps"]["http"]["servers"][SERVER]["routes"].as_array().unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0]["@id"], STOPPED_STACK_ID);
    }

    #[test]
    fn boot_config_seeds_a_share_server_on_its_own_port() {
        let _g = env_guard();
        let c = boot_config();
        let share = &c["apps"]["http"]["servers"][SHARE_SERVER];
        assert_eq!(share["listen"][0], format!(":{}", share_inner_port()));
        assert_ne!(share_inner_port(), https_port());
        assert_eq!(share["tls_connection_policies"], serde_json::json!([{}]));
        assert_eq!(share["routes"][0]["@id"], UNSHARED_ID);
        assert!(share["routes"][0].get("match").is_none(), "catch-all must not filter by host");
    }

    #[test]
    fn share_ids_never_collide_with_route_ids() {
        let id = route_id("feat-foo", "api", "web");
        assert!(id.starts_with("ws-"), "{id}");
        assert_ne!(share_id(&id), id);
        assert!(!share_id(&id).starts_with("ws-"));
    }

    #[test]
    fn count_shared_counts_only_share_routes() {
        let body = serde_json::json!([
            { "@id": share_id("ws-8_feat-foo-3_api-3_web") },
            { "@id": share_id("ws-3_bar-3_api-3_web") },
            { "@id": UNSHARED_ID },
        ])
        .to_string();
        assert_eq!(count_shared(&body), Some(2));
        assert_eq!(count_shared("null"), Some(0));
        assert_eq!(count_shared(""), None);
    }

    #[test]
    fn share_address_follows_the_flag_file() {
        let _g = env_guard();
        assert_eq!(share_address(&Site::Local), None, "no flag ⇒ not shared");
        want_share(Some("192.168.1.20")).unwrap();
        assert_eq!(share_address(&Site::Local).as_deref(), Some("192.168.1.20"));
        want_share(None).unwrap();
        assert_eq!(share_address(&Site::Local).as_deref(), Some("192.168.1.20"), "a re-share keeps the pin");
        assert_eq!(share_address(&remote_site()), None);
        drop_share();
        assert_eq!(share_address(&Site::Local), None);
    }

    #[test]
    fn parses_the_default_route_source_address() {
        let linux = "1.1.1.1 via 192.168.8.1 dev enp1s0 src 192.168.8.114 uid 1000 \n    cache \n";
        assert_eq!(parse_ip_route_src(linux).as_deref(), Some("192.168.8.114"));
        assert_eq!(parse_ip_route_src("1.1.1.1 dev wg0 table 51820"), None);
        let mac = "   route to: default\ndestination: default\n       mask: default\n    gateway: 192.168.1.1\n  interface: en0\n      flags: <UP,GATEWAY,DONE,STATIC,PRCLONING>\n";
        assert_eq!(parse_route_interface(mac).as_deref(), Some("en0"));
        assert_eq!(parse_route_interface("interface: en0; rm -rf /"), None);
    }

    #[test]
    fn cert_id_stays_one_path_segment() {
        let id = cert_id("../../config");
        assert!(!id.contains('/') && !id.contains(".."), "{id}");
    }

    #[test]
    fn stopped_stack_route_is_an_unmatched_static_page() {
        let r = stopped_stack_route();
        assert!(r.get("match").is_none(), "catch-all must not filter by host: {r}");
        assert_eq!(r["handle"][0]["handler"], "static_response");
        assert_eq!(r["handle"][0]["status_code"], 503);
        assert!(r["handle"][0]["body"].as_str().unwrap().contains("wsx up"));
    }

    #[test]
    fn https_port_env_overrides_the_default() {
        let _g = env_guard();
        std::env::set_var("WSX_HTTPS_PORT", "443");
        assert_eq!(https_port(), 443);
        std::env::remove_var("WSX_HTTPS_PORT");
        assert_eq!(https_port(), DEFAULT_HTTPS_PORT);
    }

    #[test]
    fn port_suffix_is_empty_only_for_443() {
        assert_eq!(port_suffix(443), "");
        assert_eq!(port_suffix(8443), ":8443");
    }

    #[test]
    fn https_port_ignores_a_zero_value() {
        let _g = env_guard();
        std::env::set_var("WSX_HTTPS_PORT", "0");
        assert_eq!(https_port(), DEFAULT_HTTPS_PORT);
        std::env::remove_var("WSX_HTTPS_PORT");
    }

    #[test]
    fn admin_port_seam_wins_and_zero_falls_through() {
        let _g = env_guard();
        std::env::set_var("WSX_CADDY_ADMIN_PORT", "2020");
        assert_eq!(admin_port(), 2020);
        std::env::set_var("WSX_CADDY_ADMIN_PORT", "0");
        assert_eq!(admin_port(), DEFAULT_ADMIN_PORT);
        std::env::remove_var("WSX_CADDY_ADMIN_PORT");
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
        assert!(port_bindable(&Site::Local, 8443));
        assert!(port_bindable(&Site::Local, 1024));
        let start = std::fs::read_to_string("/proc/sys/net/ipv4/ip_unprivileged_port_start")
            .ok()
            .and_then(|s| s.trim().parse::<u16>().ok());
        if let Some(start) = start {
            assert_eq!(port_bindable(&Site::Local, 443), start <= 443);
        }
    }

    /// Serializes env-mutating tests and points the config dir at a scratch dir
    /// so a machine with a persisted `--port` opt-in doesn't fail the defaults.
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
        // Lock is still held while the env is put back (Drop order: this method
        // runs before the fields) so a waiting test can't observe the scratch dir.
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
    }

    #[test]
    fn admin_listens_on_the_admin_port_over_tcp() {
        assert_eq!(boot_config()["admin"]["listen"], format!("0.0.0.0:{DEFAULT_ADMIN_PORT}"));
    }

    #[test]
    fn expected_answer_is_known_for_a_concrete_publish_address_only() {
        assert_eq!(expected_answer("127.0.0.1").as_deref(), Some("127.0.0.1"));
        assert_eq!(expected_answer("100.64.1.3").as_deref(), Some("100.64.1.3"));
        assert_eq!(expected_answer("0.0.0.0"), None);
        assert_eq!(expected_answer("::"), None);
        assert_eq!(expected_answer(""), None);
    }

    #[test]
    fn a_loopback_expectation_accepts_any_loopback_answer() {
        use std::net::IpAddr;
        let v4: IpAddr = "127.0.0.1".parse().unwrap();
        let v6: IpAddr = "::1".parse().unwrap();
        assert!(answer_ok(&v6, Some(v4), true));
        assert!(answer_ok(&v4, Some(v6), true));
        let srv: IpAddr = "100.64.1.3".parse().unwrap();
        let other: IpAddr = "100.64.1.2".parse().unwrap();
        assert!(answer_ok(&srv, Some(srv), false));
        assert!(!answer_ok(&other, Some(srv), false));
        assert!(answer_ok(&v4, None, true));
        assert!(!answer_ok(&srv, None, true));
        assert!(answer_ok(&srv, None, false));
    }

    #[test]
    fn the_hint_says_what_it_resolved_to_not_merely_that_it_didnt() {
        let dns = Dns::Wrong {
            got: "100.64.1.3".to_string(),
            expected: Some("127.0.0.1".to_string()),
        };
        let hint = resolve_hint(&Site::Local, "myapp.test", &dns).expect("a wrong answer hints");
        assert!(hint.contains("resolves to 100.64.1.3"), "{hint}");
        assert!(hint.contains("expected 127.0.0.1"), "{hint}");
        assert!(!hint.contains("doesn't resolve"), "wrong answer is not a missing one: {hint}");

        let missing = resolve_hint(&Site::Local, "myapp.test", &Dns::Unresolved).unwrap();
        assert!(missing.contains("doesn't resolve yet"), "{missing}");
        assert!(missing.contains("curl --resolve"), "{missing}");

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
    fn admin_args_hit_the_loopback_admin_port() {
        // Holds the env guard so a concurrent `WSX_CADDY_ADMIN_PORT` mutation
        // can't make the two `admin_port()` reads disagree.
        let _g = env_guard();
        let args = admin_args(&Site::Local, "/config/");
        assert_eq!(args, vec![format!("http://127.0.0.1:{}/config/", admin_port())]);
    }
}
