//! Readiness probes behind `ws remote check`.
//!
//! **Read-only** is a hard contract: a probe only observes, over the user's own
//! SSH — never installs, mutates, or `sudo`s. That is what makes `ws remote check`
//! safe to run against a server you haven't finished setting up.

use crate::remote;

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
