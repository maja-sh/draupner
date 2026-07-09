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
    ("podman", "rootless podman is up"),
    ("caddy", "the ws-caddy container is running"),
    ("workspace", "the workspace dir exists and is writable"),
];

/// A read-only shell script that probes each server prerequisite and prints one
/// `<key>:<ok|fail>` line each. Pure observation — no install, mutate, or sudo.
/// One round trip rather than an SSH connection per check. Also the exact string
/// surfaced under `-v`.
pub fn server_script(workspace_path: &str) -> String {
    let wp = remote::shell_quote(workspace_path);
    [
        "command -v mutagen >/dev/null 2>&1 && echo mutagen:ok || echo mutagen:fail".to_string(),
        "podman info >/dev/null 2>&1 && echo podman:ok || echo podman:fail".to_string(),
        // `-f name=` is an unanchored substring match, so anchor it (`^…$`) —
        // else a stray `ws-caddy-old` would make a stopped ws-caddy read as up.
        "podman ps -q -f name='^ws-caddy$' -f status=running | grep -q . && echo caddy:ok || echo caddy:fail".to_string(),
        format!("test -d {wp} && test -w {wp} && echo workspace:ok || echo workspace:fail"),
    ]
    .join("; ")
}

/// Run the server probes over SSH. Returns the per-check `Probe`s, or `None` when
/// SSH itself couldn't reach the server (so the caller reports "unreachable"
/// rather than a wall of failures).
pub fn server_probes(connection: &str, workspace_path: &str) -> Option<Vec<Probe>> {
    let script = server_script(workspace_path);
    let out = remote::ssh_capture(connection, &script)?;
    let results = parse_server(&out);
    Some(
        SERVER_CHECKS
            .iter()
            .map(|(key, desc)| {
                // A key missing from the output (older server, partial run) reads
                // as a failure, not a silent pass.
                Probe::server(key, results.get(*key).copied().unwrap_or(false), *desc)
            })
            .collect(),
    )
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

    #[test]
    fn server_script_is_read_only_and_covers_every_check() {
        let s = server_script("workspace");
        // No mutating verb / sudo — the read-only contract.
        for banned in ["sudo", "rm ", "install", "systemctl start", "podman run", "create"] {
            assert!(!s.contains(banned), "server script must stay read-only, found {banned:?}: {s}");
        }
        // Every advertised key is probed.
        for (key, _) in SERVER_CHECKS {
            assert!(s.contains(&format!("echo {key}:ok")), "missing probe for {key}: {s}");
        }
    }

    #[test]
    fn server_script_quotes_the_workspace_path() {
        let s = server_script("weird dir; rm -rf");
        assert!(s.contains("'weird dir; rm -rf'"), "workspace path must be shell-quoted: {s}");
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
        // Simulate the parse → probe mapping with a partial result set.
        let results = parse_server("mutagen:ok\npodman:ok\n");
        let workspace_ok = results.get("workspace").copied().unwrap_or(false);
        assert!(!workspace_ok, "a key absent from the server output must read as a failure");
    }
}
