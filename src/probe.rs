//! Readiness probes: the local routing prerequisites (`readiness`, read by
//! `ws setup`, `ws remote check` and the `ws up` auto-offer) and the server
//! checks behind `ws remote check`.
//!
//! **Read-only** is a hard contract: a probe only observes, over the user's own
//! SSH — never installs, mutates, or `sudo`s. That is what makes `ws remote check`
//! safe to run against a half-set-up server, and what lets `ws up` probe on its
//! hot path without importing the consented-`sudo` module (`setup.rs`), which is
//! the fixer these probes feed.

use crate::remote;
use crate::remote::Site;
use std::process::Command;

/// Which routing prerequisites the probe found satisfied.
pub struct Report {
    /// The HTTPS port Caddy is set to serve (8443 default, 443 opt-in).
    pub port: u16,
    /// Where the *client* must reach Caddy — loopback for a local workspace, the
    /// server's routable address when offloading. Carried here so every rendered
    /// line, generated command and probe agrees on one value.
    pub address: String,
    /// What `*.<domain>` resolves to, measured against [`Report::address`], so a
    /// wildcard pointing at the *wrong* machine is a distinct finding from one
    /// that doesn't resolve at all.
    pub dns: crate::caddy::Dns,
    pub ca_ok: bool,
    /// Rootless bind to the configured port is permitted. Always true unless the
    /// port is privileged (`< 1024`) and the sysctl hasn't been lowered.
    pub port_ok: bool,
}

impl Report {
    /// Drives the `ws up` auto-offer. The rootless Caddy bit isn't counted — `ws`
    /// fixes that itself without a prompt.
    pub fn privileged_gaps(&self) -> bool {
        !self.dns.is_ok() || !self.ca_ok || !self.port_ok
    }
}

/// Shared by `ws setup` and `ws remote check`'s local mode so the two never drift.
pub fn render_report(report: &Report, domain: &str) {
    // Say what a wrong wildcard *does* answer: "→ 127.0.0.1" with a cross beside
    // it reads as "not wired yet", the opposite of what a stale entry pointing at
    // another machine needs you to know.
    let detail = match &report.dns {
        crate::caddy::Dns::Wrong { got, .. } => {
            format!("*.{domain} → {} (resolves to {got})", report.address)
        }
        _ => format!("*.{domain} → {}", report.address),
    };
    crate::ui::status_line(report.dns.is_ok(), "wildcard DNS", &detail);
    crate::ui::status_line(report.ca_ok, "CA trust", "Caddy root CA in the system store");
    if report.port < 1024 {
        crate::ui::status_line(
            report.port_ok,
            "privileged port",
            &format!("rootless bind to :{}", report.port),
        );
    }
}

/// Probe the routing prerequisites without changing anything — the single owner of
/// "what counts as ready", read by both `ws setup` and the `ws up` auto-offer.
///
/// `site` is where Caddy runs, `address` where the client reaches it (see
/// [`Report::address`]); checking DNS against `address` rather than loopback is
/// what stops a remote workspace's wildcard pointing at some other machine reading
/// as ready. `known_dns` lets a caller that already resolved the wildcard skip a
/// second lookup on the `ws up` hot path.
pub fn readiness(
    site: &Site,
    runtime: &str,
    domain: &str,
    address: &str,
    known_dns: Option<crate::caddy::Dns>,
) -> Report {
    let port = crate::caddy::https_port();
    // CA trust only means something once Caddy is up to serve a cert to test.
    let caddy_up = crate::caddy::is_running(site, runtime);
    Report {
        port,
        dns: known_dns
            .unwrap_or_else(|| crate::caddy::resolve_domain(site, domain, Some(address))),
        ca_ok: caddy_up && ca_trusted(domain, port, address),
        port_ok: crate::caddy::port_bindable(site, port),
        address: address.to_string(),
    }
}

/// Asks the question that matters instead of comparing fingerprints per-platform:
/// does a TLS handshake to Caddy validate against the system store? `--resolve`
/// pins the handshake so it needs no DNS; no `-k`, so curl validates the chain and
/// succeeds iff the internal root that signed the on-demand leaf is trusted. Any
/// HTTP status counts — only a cert error or an unreachable Caddy fails it.
///
/// Runs on the **client** even when Caddy is on the server: the question is
/// whether *this* machine's trust store validates the chain.
fn ca_trusted(domain: &str, port: u16, address: &str) -> bool {
    let host = format!("ws-ca-probe.{domain}");
    Command::new("curl")
        // Short timeout: this is on the `ws up` hot path, and a wedged or
        // unreachable Caddy mustn't stall the up.
        .args(["-sS", "-o", "/dev/null", "-m", "4", "--resolve"])
        .arg(format!("{host}:{port}:{address}"))
        .arg(format!("https://{host}:{port}"))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}


pub struct Probe {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

impl Probe {
    fn new(name: &'static str, ok: bool, detail: impl Into<String>) -> Self {
        Probe { name, ok, detail: detail.into() }
    }
}

/// Built from an already-known verdict, so `ws remote check` can derive the
/// reachability line from whether the server script connected instead of paying a
/// second handshake to test it.
pub fn ssh_status(connection: &str, ok: bool) -> Probe {
    Probe::new(
        "ssh",
        ok,
        if ok {
            format!("reachable — {connection}")
        } else {
            format!("cannot open an SSH session to {connection} (host / key / Tailscale?)")
        },
    )
}

pub fn mutagen_local() -> Probe {
    let ok = crate::mutagen::installed();
    Probe::new(
        "mutagen",
        ok,
        if ok { "installed locally" } else { "not on PATH — needed to sync a remote workspace" },
    )
}

/// The keys the server script reports, paired with a human description. Kept
/// beside the script so the two can't drift.
const SERVER_CHECKS: &[(&str, &str)] = &[
    ("mutagen", "mutagen present on the server"),
    ("podman", "rootless podman is up"),
    ("caddy", "the ws-caddy container is running"),
    ("workspace", "the workspace dir exists and is writable"),
];

/// Not in `SERVER_CHECKS`: with no `domain` there is nothing to resolve, and a
/// check that can't fail shouldn't be printed.
const DNS_CHECK: &str = "dns";

/// Any label only a wildcard entry can answer for would do; this is the one
/// `server-setup/debian.sh` verifies with, so the two agree on what "wired" means.
const DNS_PROBE_LABEL: &str = "ws-probe";

/// The failure text carries the whole value of this probe, so it names what
/// didn't resolve, why anyone cares, and where the fix lives: a DHCP lease
/// renewal can rewrite `/etc/resolv.conf` and leave dnsmasq healthy but
/// unqueried, which nothing else in `ws` notices.
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

/// One `<key>:<ok|fail>` line per prerequisite, joined into a single round trip
/// and surfaced verbatim under `-v`. Observation only — no install, mutate, or
/// sudo. A `domain` of `None` leaves the wildcard check out entirely rather than
/// emitting one that always passes.
pub fn server_script(workspace_path: &str, domain: Option<&str>) -> String {
    let wp = remote::shell_quote(workspace_path);
    let mut lines = vec![
        "command -v mutagen >/dev/null 2>&1 && echo mutagen:ok || echo mutagen:fail".to_string(),
        "podman info >/dev/null 2>&1 && echo podman:ok || echo podman:fail".to_string(),
        // `-f name=` is an unanchored substring match, so anchor it (`^…$`) — else
        // a stray `ws-caddy-old` makes a stopped ws-caddy read as up.
        format!(
            "podman ps -q -f name='^{}$' -f status=running | grep -q . && echo caddy:ok || echo caddy:fail",
            crate::caddy::CADDY_CONTAINER
        ),
        format!("test -d {wp} && test -w {wp} && echo workspace:ok || echo workspace:fail"),
    ];
    if let Some(d) = domain {
        // `getent hosts` asks the server's own resolver stack, like a dev server
        // there would. Querying dnsmasq directly answers fine even while nothing
        // is pointed at it.
        let host = remote::shell_quote(&format!("{DNS_PROBE_LABEL}.{d}"));
        lines.push(format!(
            "getent hosts {host} >/dev/null 2>&1 && echo {DNS_CHECK}:ok || echo {DNS_CHECK}:fail"
        ));
    }
    lines.join("; ")
}

/// `None` when SSH itself couldn't reach the server, so the caller can report
/// "unreachable" rather than a wall of failed checks.
pub fn server_probes(connection: &str, workspace_path: &str, domain: Option<&str>) -> Option<Vec<Probe>> {
    let script = server_script(workspace_path, domain);
    let out = remote::ssh_capture(connection, &script)?;
    let results = parse_server(&out);
    // A missing key (older server, partial run) is a failure, not a silent pass.
    let verdict = |key: &str| results.get(key).copied().unwrap_or(false);
    let mut probes: Vec<Probe> = SERVER_CHECKS
        .iter()
        .map(|(key, desc)| Probe::new(key, verdict(key), *desc))
        .collect();
    if let Some(d) = domain {
        let ok = verdict(DNS_CHECK);
        probes.push(Probe::new(DNS_CHECK, ok, dns_detail(d, ok)));
    }
    Some(probes)
}

fn parse_server(stdout: &str) -> std::collections::BTreeMap<String, bool> {
    stdout
        .lines()
        .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim() == "ok")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_read_only(s: &str) {
        for banned in ["sudo", "rm ", "install", "systemctl start", "podman run", "create"] {
            assert!(!s.contains(banned), "server script must stay read-only, found {banned:?}: {s}");
        }
    }

    #[test]
    fn privileged_gaps_ignores_the_rootless_caddy_bit() {
        let mut r = Report {
            port: 8443,
            address: "127.0.0.1".to_string(),
            dns: crate::caddy::Dns::Correct,
            ca_ok: true,
            port_ok: true,
        };
        assert!(!r.privileged_gaps(), "caddy_up is rootless, not a privileged gap");
        r.dns = crate::caddy::Dns::Unresolved;
        assert!(r.privileged_gaps());
        // A wildcard pointing at the wrong machine is a gap too — it used to be
        // indistinguishable from "not wired yet", and on a remote workspace it
        // read as ready.
        r.dns = crate::caddy::Dns::Wrong {
            got: "100.64.1.3".to_string(),
            expected: Some("127.0.0.1".to_string()),
        };
        assert!(r.privileged_gaps());
    }

    #[test]
    fn server_script_is_read_only_and_covers_every_check() {
        let s = server_script("workspace", None);
        assert_read_only(&s);
        for (key, _) in SERVER_CHECKS {
            assert!(s.contains(&format!("echo {key}:ok")), "missing probe for {key}: {s}");
        }
    }

    #[test]
    fn server_script_quotes_the_workspace_path() {
        let s = server_script("weird dir; rm -rf", None);
        assert!(s.contains("'weird dir; rm -rf'"), "workspace path must be shell-quoted: {s}");
    }

    #[test]
    fn server_script_probes_dns_only_with_a_domain() {
        let without = server_script("workspace", None);
        assert!(!without.contains("dns:"), "no domain must mean no DNS check at all: {without}");

        let with = server_script("workspace", Some("myapp.test"));
        assert!(with.contains("getent hosts ws-probe.myapp.test"), "expected the wildcard lookup: {with}");
        assert!(with.contains("echo dns:ok"), "expected the dns key: {with}");
        assert!(with.contains("echo dns:fail"), "expected the dns failure branch: {with}");
        assert_read_only(&with);
    }

    #[test]
    fn server_script_quotes_the_domain() {
        let s = server_script("workspace", Some("evil; reboot"));
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
    fn server_script_probes_the_container_caddy_actually_names() {
        let s = server_script("workspace", None);
        assert!(
            s.contains(&format!("-f name='^{}$'", crate::caddy::CADDY_CONTAINER)),
            "caddy probe must use caddy::CADDY_CONTAINER, anchored: {s}"
        );
    }

    #[test]
    fn parse_server_reads_ok_and_fail() {
        let m = parse_server("mutagen:ok\npodman:fail\ncaddy:ok\n");
        assert_eq!(m.get("mutagen"), Some(&true));
        assert_eq!(m.get("podman"), Some(&false));
        assert_eq!(m.get("caddy"), Some(&true));
        assert_eq!(m.get("workspace"), None);
    }

    #[test]
    fn server_probes_default_a_missing_key_to_fail() {
        // Mirrors the verdict closure in `server_probes`, which needs SSH to run.
        let results = parse_server("mutagen:ok\npodman:ok\n");
        let workspace_ok = results.get("workspace").copied().unwrap_or(false);
        assert!(!workspace_ok, "a key absent from the server output must read as a failure");
    }
}
