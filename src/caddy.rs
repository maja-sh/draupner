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
//! **Slice 6.2 caveat:** Caddy binds a *non-privileged* HTTPS port here. Rootless
//! bind to `:443` needs a sysctl that's slice 6.3's (consented, one-time)
//! privileged step; until then, reach a route with
//! `curl --resolve <host>:<HTTPS_PORT>:127.0.0.1 https://<host>:<HTTPS_PORT>`.
//!
//! **Admin API is a unix socket, never TCP.** Caddy sits on `ws-net` (to dial
//! the worktree containers), and the admin API grants total control of the proxy
//! with no auth — so binding it to a TCP port would expose it to every tenant
//! container on `ws-net` (a route-hijack / SSRF pivot). Instead the admin listens
//! on a unix socket in a host-mounted dir: reachable by the host user (`ws`)
//! over `curl --unix-socket`, unreachable from any network.
//!
//! Seams: `WS_CADDY_ADMIN` (when set, `ws` talks to that admin URL and treats
//! Caddy + `ws-net` as externally managed — BYO / test stub) and the runtime
//! binary let tests substitute fakes.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Shared user-defined network every worktree's stack and Caddy join.
pub const WS_NET: &str = "ws-net";
/// Container name for the singleton Caddy.
pub const CADDY_CONTAINER: &str = "ws-caddy";
/// Pinned Caddy image.
const CADDY_IMAGE: &str = "docker.io/library/caddy:2";
/// The HTTP server block name inside Caddy's config (routes hang off it).
const SERVER: &str = "ws";
/// HTTPS listen port. Non-privileged for now; slice 6.3 moves it to 443 once the
/// `net.ipv4.ip_unprivileged_port_start` sysctl is consented.
pub const HTTPS_PORT: u16 = 8443;
/// Mount point inside the Caddy container holding the pinned config and the
/// admin unix socket — bind-mounted from `config_dir()` so the host reaches the
/// socket while `ws-net` cannot.
const SOCK_MOUNT: &str = "/sock";

/// The curl args to reach the admin API for `path`: over the unix socket
/// (default — no TCP interface, so `ws-net` tenants can't reach it), or the
/// `WS_CADDY_ADMIN` URL verbatim (BYO / test seam).
fn admin_args(path: &str) -> Vec<String> {
    if let Ok(url) = std::env::var("WS_CADDY_ADMIN") {
        vec![format!("{}{}", url.trim_end_matches('/'), path)]
    } else {
        vec![
            "--unix-socket".to_string(),
            socket_path().to_string_lossy().into_owned(),
            format!("http://localhost{path}"),
        ]
    }
}

/// Whether Caddy + `ws-net` are managed outside `ws` — signalled by an explicit
/// `WS_CADDY_ADMIN`. In that mode `ws` still adds/removes routes against the
/// given admin API but does not create the network or run the container (you
/// bring your own). Tests set it to a dead endpoint so a sweep never spins up
/// real infra.
fn externally_managed() -> bool {
    std::env::var_os("WS_CADDY_ADMIN").is_some()
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
}

/// Ensure the shared `ws-net` exists (idempotent rootless create).
pub fn ensure_net(runtime: &str) -> Result<()> {
    if externally_managed() {
        return Ok(());
    }
    let out = Command::new(runtime)
        .args(["network", "create", WS_NET])
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
pub fn ensure_running(runtime: &str) -> Result<()> {
    if externally_managed() || container_running(runtime, CADDY_CONTAINER) {
        return Ok(());
    }
    ensure_net(runtime)?;
    let dir = write_boot_config()?;
    // A stale admin socket from a prior run would make Caddy's bind fail; we're
    // the only one starting (container isn't running), so clearing it is safe.
    let _ = std::fs::remove_file(dir.join("admin.sock"));

    if run_caddy(runtime, &dir) {
        wait_admin_ready();
        return Ok(());
    }
    // The run failed — either a peer already started it (name taken, now
    // running → done) or a stale stopped container holds the name (remove it,
    // never a running one, and retry once).
    if container_running(runtime, CADDY_CONTAINER) {
        return Ok(());
    }
    let _ = Command::new(runtime).args(["rm", "-f", CADDY_CONTAINER]).output();
    if run_caddy(runtime, &dir) || container_running(runtime, CADDY_CONTAINER) {
        wait_admin_ready();
        return Ok(());
    }
    bail!("could not start {CADDY_CONTAINER}")
}

/// `<runtime> run -d` the Caddy container. `dir` (the host config dir) is mounted
/// so Caddy reads its config and writes the admin socket where the host can reach
/// it. Only the HTTPS data-plane port is published — the admin API stays on the
/// unix socket, off every network. Returns whether it started; a name clash (a
/// peer's container) or any other failure is `false`, not an error.
fn run_caddy(runtime: &str, dir: &Path) -> bool {
    Command::new(runtime)
        .args(["run", "-d", "--name", CADDY_CONTAINER, "--network", WS_NET, "--restart", "always"])
        .arg("-p").arg(format!("127.0.0.1:{HTTPS_PORT}:{HTTPS_PORT}"))
        .arg("-v").arg(format!("{}:{SOCK_MOUNT}:Z", dir.display()))
        .arg(CADDY_IMAGE)
        .args(["caddy", "run", "--config"]).arg(format!("{SOCK_MOUNT}/caddy.json"))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Poll the admin API until it answers — Caddy's listener isn't up the instant
/// `run -d` returns, so the immediately-following `route_up` could otherwise hit
/// a not-yet-listening socket. Best-effort with a short budget.
fn wait_admin_ready() {
    for _ in 0..50 {
        let mut args = vec!["-o".to_string(), "/dev/null".to_string()];
        args.extend(admin_args("/config/"));
        if curl(&args).map(|o| o.ok).unwrap_or(false) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Whether a container by this name is running.
fn container_running(runtime: &str, name: &str) -> bool {
    Command::new(runtime)
        .args(["container", "inspect", "-f", "{{.State.Running}}", name])
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true")
        .unwrap_or(false)
}

/// Where the pinned Caddy boot config lives (machine-global — one Caddy serves
/// every workspace on the box). `$XDG_CONFIG_HOME/ws/` or `~/.config/ws/`.
fn config_dir() -> PathBuf {
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        if !x.is_empty() {
            return PathBuf::from(x).join("ws");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config").join("ws")
}

/// The host path of the admin unix socket (Caddy writes it into the mounted
/// config dir). Canonicalized so it matches the absolute bind-mount source; the
/// dir exists by the time any admin call runs (`ensure_running` created it).
fn socket_path() -> PathBuf {
    let dir = config_dir();
    std::fs::canonicalize(&dir).unwrap_or(dir).join("admin.sock")
}

/// Write the pinned boot config (empty routes; everything else flows in over the
/// admin API) and return the absolute config dir (mounted into the container).
fn write_boot_config() -> Result<PathBuf> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    // The bind-mount source must be absolute — a relative path (HOME unset →
    // `./.config/ws`) is rejected by the runtime. Canonicalize now that it exists.
    let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
    crate::config::write_json_pretty(&dir.join("caddy.json"), &boot_config())?;
    Ok(dir)
}

/// The Caddy boot config: admin API on a **unix socket** (off every network —
/// see the module doc), one HTTPS server with an empty route set, and `tls
/// internal` (Caddy is the local CA).
fn boot_config() -> serde_json::Value {
    serde_json::json!({
        "admin": { "listen": format!("unix/{SOCK_MOUNT}/admin.sock") },
        "apps": {
            "http": {
                "https_port": HTTPS_PORT,
                "servers": {
                    SERVER: { "listen": [format!(":{HTTPS_PORT}")], "routes": [] }
                }
            },
            "tls": { "automation": { "policies": [ { "issuers": [ { "module": "internal" } ] } ] } }
        }
    })
}

/// The route object Caddy stores: match the worktree hostname, reverse-proxy to
/// its upstream (`<project>-<service>-1:<port>` for a container, or
/// `host.containers.internal:<port>` for a host process).
fn route(id: &str, hostname: &str, upstream: &str) -> serde_json::Value {
    serde_json::json!({
        "@id": id,
        "match": [ { "host": [ hostname ] } ],
        "handle": [ {
            "handler": "reverse_proxy",
            "upstreams": [ { "dial": upstream } ]
        } ]
    })
}

/// Add (or replace) the route for a worktree service. Deletes any prior route
/// with the same `@id` first so a re-`up` with a changed upstream takes effect,
/// then appends the fresh one — both scoped operations, never `/load`.
pub fn route_up(id: &str, hostname: &str, upstream: &str) -> Result<()> {
    delete_id(id); // ignore-absent
    let body = route(id, hostname, upstream).to_string();
    let mut args = vec![
        "-X".to_string(), "POST".to_string(),
        "-H".to_string(), "Content-Type: application/json".to_string(),
        "-d".to_string(), body,
    ];
    args.extend(admin_args(&format!("/config/apps/http/servers/{SERVER}/routes")));
    check(&curl(&args)?, || format!("add route {id}"))
}

/// Remove a worktree service's route. A missing route, or an unreachable Caddy,
/// is fine — teardown is best-effort (infallible by design).
pub fn route_down(id: &str) {
    delete_id(id);
}

fn delete_id(id: &str) {
    // Best-effort: a 404 (no such route yet) is expected on first up / double down.
    let mut args = vec!["-X".to_string(), "DELETE".to_string()];
    args.extend(admin_args(&format!("/id/{id}/")));
    let _ = curl(&args);
}

/// Run `curl` against the admin API, capturing the response and HTTP status.
fn curl(args: &[String]) -> Result<CurlOut> {
    let out = Command::new("curl")
        .args(["-sS", "-m", "10", "-w", "\n%{http_code}"])
        .args(args)
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

    #[test]
    fn route_id_is_stable_and_namespaced() {
        assert_eq!(route_id("feat-foo", "api", "web"), "ws-feat-foo-api-web");
        // Same service name in two repos → distinct ids (no cross-repo clobber).
        assert_ne!(route_id("feat-foo", "api", "web"), route_id("feat-foo", "ui", "web"));
    }

    #[test]
    fn route_json_matches_host_and_proxies_upstream() {
        let r = route("ws-feat-foo-web", "feat-foo.web.myapp.test", "feat-foo-api-web-1:80");
        assert_eq!(r["@id"], "ws-feat-foo-web");
        assert_eq!(r["match"][0]["host"][0], "feat-foo.web.myapp.test");
        assert_eq!(r["handle"][0]["handler"], "reverse_proxy");
        assert_eq!(r["handle"][0]["upstreams"][0]["dial"], "feat-foo-api-web-1:80");
    }

    #[test]
    fn boot_config_has_internal_tls_and_empty_routes() {
        let c = boot_config();
        assert_eq!(c["apps"]["tls"]["automation"]["policies"][0]["issuers"][0]["module"], "internal");
        assert_eq!(c["apps"]["http"]["servers"][SERVER]["routes"].as_array().unwrap().len(), 0);
        assert_eq!(c["apps"]["http"]["servers"][SERVER]["listen"][0], format!(":{HTTPS_PORT}"));
    }

    #[test]
    fn admin_is_a_unix_socket_not_tcp() {
        // Security: the admin API must never be on a TCP interface (it would be
        // reachable from `ws-net` tenants). It listens on a unix socket only.
        let listen = boot_config()["admin"]["listen"].as_str().unwrap().to_string();
        assert!(listen.starts_with("unix/"), "admin must be a unix socket, got {listen}");
        assert!(!listen.contains("0.0.0.0") && !listen.contains(':'), "no TCP admin: {listen}");
    }
}
