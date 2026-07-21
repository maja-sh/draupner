//! Host setup for per-worktree routing — readiness probe + consented fixes.
//!
//! Routing needs a handful of one-time host bits. `ws` splits them by privilege
//! and by role, and **never runs `sudo` silently**:
//!
//! - **Rootless** (`ws-net`, the Caddy container): `ws` just does these — no
//!   privilege, no prompt.
//! - **Privileged** (wildcard DNS `*.<domain>` → loopback, trusting Caddy's root
//!   CA, and — only if you opt into `:443` — the `ip_unprivileged_port_start`
//!   sysctl): these are shown and consented, never silent.
//!
//! How the privileged bits are delivered differs by host, matching where each
//! platform's setup is best expressed:
//!
//! - **macOS host** (the workspace owner's machine): `ws setup` walks each gap
//!   interactively — show the exact command, `[R]un / [S]kip / show-me`. `[R]un`
//!   shells the command (with its `sudo`) inheriting the terminal, so `sudo`
//!   prompts for the password directly — `ws` never reads or stores it.
//! - **Linux host / remote offload box**: the privileged bits live in a
//!   versioned, readable `server-setup/<distro>.sh` the user runs themselves (the
//!   script *is* the transparency). `ws setup` here reports the gaps and points
//!   at the script rather than driving `sudo` itself.
//!
//! Non-interactive (stdin not a TTY): print the commands and bail — never hang on
//! a `sudo` password prompt. `--yes` runs the macOS steps unattended.
//!
//! The probe is non-mutating and also drives the auto-offer on `ws up` (a nudge
//! when a privileged bit is missing).

use anyhow::{bail, Context, Result};
use std::process::Command;

/// The host platform `ws` is running on, deciding how privileged bits are
/// delivered (macOS → interactive command-prompt; Linux → the setup script).
enum Platform {
    MacOs,
    Linux,
    Other,
}

/// Detect the host platform from the compile target (the binary is built per
/// host, so this is authoritative).
fn platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::MacOs
    } else if cfg!(target_os = "linux") {
        Platform::Linux
    } else {
        Platform::Other
    }
}

/// Result of the readiness probe: which routing prerequisites are satisfied.
pub struct Report {
    /// The HTTPS port Caddy is set to serve (8443 default, 443 opt-in).
    pub port: u16,
    /// `*.<domain>` resolves to loopback (browser can find worktree hosts).
    pub dns_ok: bool,
    /// Caddy's internal root CA is trusted by the system store (no cert warning).
    pub ca_ok: bool,
    /// Rootless bind to the configured port is permitted. Always true unless the
    /// port is privileged (`< 1024`) and the sysctl hasn't been lowered.
    pub port_ok: bool,
}

impl Report {
    /// Whether any *privileged* prerequisite is still missing (drives the
    /// `ws up` auto-offer). The rootless Caddy bit isn't counted — `ws` fixes
    /// that itself without a prompt.
    pub fn privileged_gaps(&self) -> bool {
        !self.dns_ok || !self.ca_ok || !self.port_ok
    }
}

/// Render a probe report as readiness status lines — shared by `ws setup` and
/// `ws remote check`'s local mode so the two never drift.
pub fn render_report(report: &Report, domain: &str) {
    crate::ui::status_line(report.dns_ok, "wildcard DNS", &format!("*.{domain} → 127.0.0.1"));
    crate::ui::status_line(report.ca_ok, "CA trust", "Caddy root CA in the system store");
    if report.port < 1024 {
        crate::ui::status_line(
            report.port_ok,
            "privileged port",
            &format!("rootless bind to :{}", report.port),
        );
    }
}

/// Probe the routing prerequisites without changing anything. Safe to call on
/// any `ws up`. `known_dns` lets a caller that already resolved the wildcard
/// (the `ws up` auto-offer learns it from `ensure_routing`) pass the answer in,
/// so the hot path doesn't resolve DNS a second time; `None` resolves it here.
/// The single owner of "what counts as ready" — the auto-offer and `ws setup`
/// both read the resulting `Report`.
pub fn probe(runtime: &str, domain: &str, known_dns: Option<bool>) -> Report {
    // `ws setup` is a host-role command: it wires the *client's* DNS/CA/port, so
    // it always probes the local site (remote setup is a later slice).
    let site = crate::remote::Site::Local;
    let port = crate::caddy::https_port();
    // CA trust only means something once Caddy is up to serve a cert to test.
    let caddy_up = crate::caddy::is_running(&site, runtime);
    Report {
        port,
        dns_ok: known_dns.unwrap_or_else(|| crate::caddy::domain_resolves(&site, domain)),
        ca_ok: caddy_up && ca_trusted(domain, port),
        port_ok: crate::caddy::port_bindable(&site, port),
    }
}

/// Whether the system trust store already trusts Caddy's internal root. Rather
/// than extract the cert and compare fingerprints per-platform, ask the question
/// that actually matters: does a TLS handshake to the local Caddy **validate**
/// against the system store? `--resolve` avoids needing DNS; no `-k`, so curl
/// validates the chain; on-demand TLS mints a leaf for the probe host signed by
/// the internal root, so curl succeeds iff that root is trusted. Any HTTP status
/// (even the 503 stopped-stack page) counts — only a cert error (curl 60) or an
/// unreachable Caddy fails it.
fn ca_trusted(domain: &str, port: u16) -> bool {
    let host = format!("ws-ca-probe.{domain}");
    Command::new("curl")
        // Short timeout: a real loopback handshake is milliseconds, and this runs
        // on the `ws up` hot path — a wedged Caddy mustn't stall the up.
        .args(["-sS", "-o", "/dev/null", "-m", "4", "--resolve"])
        .arg(format!("{host}:{port}:127.0.0.1"))
        .arg(format!("https://{host}:{port}"))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A privileged step: what it does + the exact shell command(s) to show and run.
/// The commands embed their own `sudo` so the user sees precisely what elevates.
struct Step {
    what: String,
    commands: Vec<String>,
}

/// Whether a domain is safe to interpolate into a shell command — the only
/// externally-sourced value that reaches the generated setup commands. Hostnames
/// are `[a-z0-9.-]`; anything else (spaces, shell metacharacters) is refused so a
/// hand-edited `.ws.json` can't smuggle a command into a consented `sudo` line.
fn valid_domain(domain: &str) -> bool {
    !domain.is_empty()
        && domain.len() <= 253
        && domain.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

/// The macOS command to wire `*.<domain>` → 127.0.0.1: a dnsmasq wildcard plus a
/// `/etc/resolver/<domain>` entry pointing the private TLD at it.
fn dns_step_macos(domain: &str) -> Step {
    Step {
        what: format!("resolve `*.{domain}` → 127.0.0.1 (dnsmasq + /etc/resolver)"),
        commands: vec![
            "brew install dnsmasq".to_string(),
            format!("echo 'address=/{domain}/127.0.0.1' >> \"$(brew --prefix)/etc/dnsmasq.conf\""),
            "sudo brew services start dnsmasq".to_string(),
            "sudo mkdir -p /etc/resolver".to_string(),
            format!("echo 'nameserver 127.0.0.1' | sudo tee /etc/resolver/{domain}"),
        ],
    }
}

/// The macOS command to trust Caddy's root CA in the System keychain. `pem_path`
/// is where `ws` wrote the fetched root certificate — single-quoted, since it's a
/// filesystem path (not a validated hostname) and the whole line is later run via
/// `sh -c`; an unusual `$HOME`/dir must not split the command or inject.
fn ca_step_macos(pem_path: &str) -> Step {
    Step {
        what: "trust Caddy's root CA in the System keychain".to_string(),
        commands: vec![format!(
            "sudo security add-trusted-cert -d -r trustRoot \
             -k /Library/Keychains/System.keychain {}",
            sh_quote(pem_path)
        )],
    }
}

/// Single-quote a string for safe interpolation into an `sh -c` command line
/// (embedded single quotes become `'\''`). Used for filesystem paths that aren't
/// otherwise validated.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Run `ws setup`: ensure the rootless infra directly, then walk each privileged
/// gap per the host's delivery model. `port` (from `--port`) opts into a
/// non-default HTTPS port (443 for clean URLs) and is persisted before Caddy is
/// (re-)seeded so the change takes effect.
pub fn run(
    domain: Option<&str>,
    runtime: &str,
    yes: bool,
    port: Option<u16>,
    publish_addr: &str,
) -> Result<()> {
    crate::ui::header("ws-setup", domain.unwrap_or("(no domain)"));

    if let Some(p) = port {
        crate::caddy::set_https_port(p)?;
        crate::ui::note(&format!("HTTPS port set to {p} (persisted)"));
    }

    // Rootless bits — no privilege, no prompt. ensure_running also re-seeds Caddy
    // if the port just changed.
    let step = crate::ui::step("caddy", "rootless network + Caddy container");
    match crate::caddy::ensure_running(&crate::remote::Site::Local, runtime, publish_addr) {
        Ok(()) => step.ok(),
        Err(e) => {
            step.fail(&format!("{e}"));
            return Err(crate::ops::op_failure(format!("rootless Caddy setup failed: {e:#}")));
        }
    }

    let Some(domain) = domain else {
        crate::ui::summary_ok(&["No `domain` set — routing isn't configured, so there are no \
                                 privileged bits to set up. Add `domain` to .ws.json to route \
                                 worktrees.".to_string()]);
        return Ok(());
    };
    if !valid_domain(domain) {
        return Err(crate::ops::op_failure(format!(
            "refusing to build setup commands for an unsafe domain `{domain}` \
             (expected a hostname of [a-z0-9.-])"
        )));
    }

    let report = probe(runtime, domain, None);
    let plat = platform();

    render_report(&report, domain);

    if !report.privileged_gaps() {
        crate::ui::summary_ok(&[format!(
            "routing is ready — reach a worktree at https://<branch>.<svc>.{domain}{}",
            port_suffix(report.port)
        )]);
        return Ok(());
    }

    // Deliver the gaps per host model.
    match plat {
        Platform::MacOs => apply_macos_gaps(domain, &report, yes),
        Platform::Linux => {
            guide_linux_gaps(&report);
            Ok(())
        }
        Platform::Other => {
            crate::ui::note(
                "automated setup isn't available on this platform yet — wire \
                 `*.<domain>` → 127.0.0.1 and trust Caddy's root CA manually.",
            );
            Ok(())
        }
    }
}

/// Walk the macOS privileged gaps interactively (or unattended with `--yes`).
fn apply_macos_gaps(domain: &str, report: &Report, yes: bool) -> Result<()> {
    let mut steps: Vec<Step> = Vec::new();
    if !report.dns_ok {
        steps.push(dns_step_macos(domain));
    }
    if !report.ca_ok {
        // Fetch + persist the root CA (to ws's own config dir, not $TMPDIR) so the
        // trust command has a concrete, non-clobberable file to point at.
        match crate::caddy::write_root_ca(&crate::remote::Site::Local) {
            Ok(path) => steps.push(ca_step_macos(&path.to_string_lossy())),
            Err(e) => crate::ui::note(&format!(
                "couldn't fetch Caddy's root CA to set up trust ({e:#}) — is Caddy running?"
            )),
        }
    }
    if report.port < 1024 && !report.port_ok {
        crate::ui::note(&format!(
            "binding :{} on macOS needs a privileged listener — not automated yet; \
             use the default :8443 or run Caddy privileged.",
            report.port
        ));
    }

    let mut printed = false;
    for step in &steps {
        printed |= apply_step(step, yes)?;
    }

    if printed {
        crate::ui::nudge(
            "stdin isn't a terminal — run the commands above, or re-run `ws setup` in a terminal.",
        );
    } else {
        crate::ui::summary_ok(&["setup walk complete — re-run `ws setup` to re-check.".to_string()]);
    }
    Ok(())
}

/// Handle one macOS step: `--yes` runs it; otherwise prompt `[R]un/[S]kip/?`.
/// Returns whether the commands were *printed* (the non-interactive bail) rather
/// than run or skipped — the caller uses that to decide the closing nudge.
fn apply_step(step: &Step, yes: bool) -> Result<bool> {
    crate::ui::status_line(false, &step.what, "needs one privileged command");
    if yes {
        run_commands(&step.commands)?;
        return Ok(false);
    }
    loop {
        let Some(ans) = crate::ui::ask("[R]un / [S]kip / show-me? (r/s/?)") else {
            // Non-TTY: print and bail rather than hang on a sudo prompt.
            crate::ui::command_block(&step.commands);
            return Ok(true);
        };
        match ans.to_lowercase().as_str() {
            "r" | "run" | "y" | "yes" | "" => {
                run_commands(&step.commands)?;
                return Ok(false);
            }
            "s" | "skip" | "n" | "no" => return Ok(false),
            "?" | "show" | "show-me" => crate::ui::command_block(&step.commands),
            _ => {} // re-prompt
        }
    }
}

/// Run each command via `sh -c`, inheriting the terminal so `sudo` prompts
/// directly (`ws` never reads the password). Stops at the first failure.
fn run_commands(commands: &[String]) -> Result<()> {
    for c in commands {
        crate::ui::note(&format!("$ {c}"));
        let status = Command::new("sh")
            .arg("-c")
            .arg(c)
            .status()
            .with_context(|| format!("run `{c}`"))?;
        if !status.success() {
            bail!("`{c}` exited with {status}");
        }
    }
    Ok(())
}

/// On a Linux host the privileged bits live in the versioned setup script — point
/// at it rather than driving `sudo` from `ws`.
fn guide_linux_gaps(report: &Report) {
    let mut needs = Vec::new();
    if !report.dns_ok {
        needs.push("wildcard DNS (*.<domain> → 127.0.0.1)");
    }
    if !report.ca_ok {
        needs.push("trust Caddy's root CA");
    }
    if report.port < 1024 && !report.port_ok {
        needs.push("allow rootless bind to the privileged port (sysctl)");
    }
    crate::ui::note(&format!("missing: {}", needs.join("; ")));
    crate::ui::nudge(
        "run the setup script for your distro — `server-setup/debian.sh` \
         (Debian/Ubuntu) — it applies the privileged bits transparently.",
    );
}

/// The `:port` suffix a browser URL carries, empty for the standard `:443`.
fn port_suffix(port: u16) -> String {
    if port == 443 {
        String::new()
    } else {
        format!(":{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_domain_accepts_hostnames_rejects_shell_metacharacters() {
        assert!(valid_domain("myapp.test"));
        assert!(valid_domain("my-app.test"));
        assert!(!valid_domain(""));
        assert!(!valid_domain("myapp.test; rm -rf /"));
        assert!(!valid_domain("$(whoami).test"));
        assert!(!valid_domain("a b.test"));
    }

    #[test]
    fn macos_dns_step_wires_the_wildcard_and_resolver() {
        let step = dns_step_macos("myapp.test");
        let joined = step.commands.join("\n");
        assert!(joined.contains("address=/myapp.test/127.0.0.1"), "{joined}");
        assert!(joined.contains("/etc/resolver/myapp.test"), "{joined}");
    }

    #[test]
    fn macos_ca_step_trusts_the_root_in_the_system_keychain() {
        let step = ca_step_macos("/tmp/ws-caddy-root.crt");
        let cmd = &step.commands[0];
        assert!(cmd.contains("security add-trusted-cert"), "{cmd}");
        assert!(cmd.contains("/Library/Keychains/System.keychain"), "{cmd}");
        assert!(cmd.contains("/tmp/ws-caddy-root.crt"), "{cmd}");
    }

    #[test]
    fn privileged_gaps_ignores_the_rootless_caddy_bit() {
        let mut r = Report { port: 8443, dns_ok: true, ca_ok: true, port_ok: true };
        assert!(!r.privileged_gaps(), "caddy_up is rootless, not a privileged gap");
        r.dns_ok = false;
        assert!(r.privileged_gaps());
    }

    #[test]
    fn sh_quote_neutralizes_metacharacters() {
        assert_eq!(sh_quote("/tmp/x.crt"), "'/tmp/x.crt'");
        // A path with a shell metacharacter is contained inside the quotes.
        assert_eq!(sh_quote("/t;rm -rf/x"), "'/t;rm -rf/x'");
        // An embedded single quote is escaped, not a break-out.
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn port_suffix_is_empty_only_for_443() {
        assert_eq!(port_suffix(443), "");
        assert_eq!(port_suffix(8443), ":8443");
    }

    #[test]
    fn apply_step_prints_and_bails_when_stdin_is_not_a_tty() {
        // Under `cargo test` stdin isn't a terminal, so the [R]un/[S]kip prompt
        // can't be shown — the step must print its commands and report `Printed`
        // (never block on a sudo prompt), and must NOT try to run anything.
        let step = ca_step_macos("/tmp/x.crt");
        assert!(apply_step(&step, false).unwrap(), "non-interactive step must print (bail), not run");
    }
}
