//! Host setup for per-worktree routing — readiness probe + consented fixes.
//!
//! Routing needs a handful of one-time host bits. `ws` splits them by privilege
//! and by role, and **never runs `sudo` silently**:
//!
//! - **Rootless** (`ws-net`, the Caddy container): `ws` just does these — no
//!   privilege, no prompt.
//! - **Privileged** (wildcard DNS `*.<domain>` → the routing address, trusting
//!   Caddy's root CA, and — only if you opt into `:443` — the
//!   `ip_unprivileged_port_start` sysctl): these are shown and consented, never
//!   silent.
//!
//! Every step is parameterised by **where the routing data plane lives**: a
//! `Site` (the local host, or the offload server over SSH) plus the address the
//! *client* must reach it at. Locally that address is loopback; for a
//! remote-offload workspace it's the server's routable address, and the client
//! still needs both bits — DNS pointing at the server and the server's CA
//! trusted — or the browser can't reach or trust a worktree URL.
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

use crate::remote::Site;

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
    /// The address the client must resolve `*.<domain>` to and reach Caddy at —
    /// loopback for a local workspace, the server's routable address when
    /// offloading. Carried on the report so every rendered line, generated
    /// command and probe agrees on one value.
    pub address: String,
    /// `*.<domain>` resolves to [`Report::address`] (browser can find worktree
    /// hosts).
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
    crate::ui::status_line(
        report.dns_ok,
        "wildcard DNS",
        &format!("*.{domain} → {}", report.address),
    );
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
/// any `ws up`. `site` is where Caddy runs and `address` is where the client
/// reaches it (see [`Report::address`]) — for a local workspace those are the
/// local host and loopback; for a remote one, the server and its routable
/// address. `known_dns` lets a caller that already resolved the wildcard (the
/// `ws up` auto-offer learns it from `ensure_routing`) pass the answer in, so the
/// hot path doesn't resolve DNS a second time; `None` resolves it here. The
/// single owner of "what counts as ready" — the auto-offer and `ws setup` both
/// read the resulting `Report`.
pub fn probe(
    site: &Site,
    runtime: &str,
    domain: &str,
    address: &str,
    known_dns: Option<bool>,
) -> Report {
    let port = crate::caddy::https_port();
    // CA trust only means something once Caddy is up to serve a cert to test.
    let caddy_up = crate::caddy::is_running(site, runtime);
    Report {
        port,
        dns_ok: known_dns.unwrap_or_else(|| crate::caddy::domain_resolves(site, domain)),
        ca_ok: caddy_up && ca_trusted(domain, port, address),
        port_ok: crate::caddy::port_bindable(site, port),
        address: address.to_string(),
    }
}

/// Whether the system trust store already trusts Caddy's internal root. Rather
/// than extract the cert and compare fingerprints per-platform, ask the question
/// that actually matters: does a TLS handshake to Caddy **validate** against the
/// system store? `--resolve` pins the handshake at `address` so it needs no DNS;
/// no `-k`, so curl validates the chain; on-demand TLS mints a leaf for the probe
/// host signed by the internal root, so curl succeeds iff that root is trusted.
/// Any HTTP status (even the 503 stopped-stack page) counts — only a cert error
/// (curl 60) or an unreachable Caddy fails it.
///
/// The curl always runs **on the client**, even when Caddy is on the server:
/// the question is whether *this* machine's trust store validates the chain.
fn ca_trusted(domain: &str, port: u16, address: &str) -> bool {
    let host = format!("ws-ca-probe.{domain}");
    Command::new("curl")
        // Short timeout: a loopback handshake is milliseconds and a LAN/tailnet one
        // barely more, and this runs on the `ws up` hot path — a wedged or
        // unreachable Caddy mustn't stall the up.
        .args(["-sS", "-o", "/dev/null", "-m", "4", "--resolve"])
        .arg(format!("{host}:{port}:{address}"))
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

/// Whether a domain is safe to interpolate into a shell command — hostnames are
/// `[a-z0-9.-]`; anything else (spaces, shell metacharacters) is refused so a
/// hand-edited `.ws.json` can't smuggle a command into a consented `sudo` line.
fn valid_domain(domain: &str) -> bool {
    !domain.is_empty()
        && domain.len() <= 253
        && domain.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

/// Whether a routing address is safe to interpolate into a shell command. The
/// other externally-sourced value reaching a generated `sudo` line: it comes from
/// `remote.publish_address` in a possibly-cloned `.ws.json`, or from `ssh -G`
/// output. **Must parse as a literal IP** — dnsmasq's `address=/domain/<answer>`
/// wants an address, not a name, so this is the real format requirement and not
/// merely a safety net; an IP is also inert in a shell.
fn valid_address(address: &str) -> bool {
    address.parse::<std::net::IpAddr>().is_ok()
}

/// The macOS command to wire `*.<domain>` → `address`: a dnsmasq wildcard plus a
/// `/etc/resolver/<domain>` entry pointing the private TLD at it. `address` is the
/// wildcard's *answer* (loopback locally, the offload server otherwise); the
/// resolver entry always points at dnsmasq itself on loopback, which is what
/// serves that answer.
///
/// The wildcard is **replaced, not appended**. A plain `>>` is wrong whenever a
/// line for this domain already exists — flipping a workspace local↔remote, or
/// changing `remote.publish_address`, is precisely when you re-run this — and
/// dnsmasq answers with *every* matching `address=` line, so a stale loopback
/// entry alongside the new one means the browser can still be handed 127.0.0.1
/// and fail. `sed -i.ws-bak` takes its suffix attached, which is the form both
/// BSD (macOS) and GNU sed accept, and leaves the prior file recoverable.
fn dns_step_macos(domain: &str, address: &str) -> Step {
    let conf = "\"$(brew --prefix)/etc/dnsmasq.conf\"";
    // Dots are regex metacharacters; the domain is `[a-z0-9.-]`-validated, so
    // escaping them is the only quoting the pattern needs.
    let pattern = domain.replace('.', "\\.");
    Step {
        what: format!("resolve `*.{domain}` → {address} (dnsmasq + /etc/resolver)"),
        commands: vec![
            "brew install dnsmasq".to_string(),
            format!("sed -i.ws-bak '/^address=\\/{pattern}\\//d' {conf}"),
            format!("echo 'address=/{domain}/{address}' >> {conf}"),
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
///
/// `site` is where the routing data plane lives and `address` is where the client
/// reaches it. The rootless Caddy is ensured **at the site** — locally for a local
/// workspace, on the server over SSH for an offload one. It is `ws`-owned state in
/// both cases (nothing in `server-setup/<distro>.sh` starts it), and readying it is
/// exactly this command's job: without it the CA fetch below has nothing to fetch
/// from, and `ws remote check` reports a failure only `ws up` could clear.
///
/// What is *not* done for a remote workspace is starting a **second, local** Caddy:
/// that one really would be pointless, and it could fight for `:443`.
///
/// The privileged client-side bits (DNS, CA trust) run either way — they're what
/// makes the server's routes reachable and trusted from *this* machine.
pub fn run(
    site: &Site,
    domain: Option<&str>,
    runtime: &str,
    yes: bool,
    port: Option<u16>,
    address: &str,
    publish_addr: &str,
) -> Result<()> {
    crate::ui::header("ws-setup", domain.unwrap_or("(no domain)"));

    if let Some(p) = port {
        crate::caddy::set_https_port(p)?;
        crate::ui::note(&format!("HTTPS port set to {p} (persisted)"));
    }

    if site.is_remote() {
        crate::ui::note(&format!(
            "this workspace offloads to {address} — readying Caddy there, then this \
             machine's DNS + CA trust so the routes are reachable from here."
        ));
    }

    // Rootless bits — no privilege, no prompt — at whichever site routing lives.
    // ensure_running also re-seeds Caddy if the port just changed.
    let step = crate::ui::step(
        "caddy",
        if site.is_remote() {
            "network + Caddy container on the server"
        } else {
            "rootless network + Caddy container"
        },
    );
    match crate::caddy::ensure_running(site, runtime, publish_addr) {
        Ok(()) => step.ok(),
        Err(e) => {
            step.fail(&format!("{e}"));
            return Err(crate::ops::op_failure(format!("Caddy setup failed: {e:#}")));
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
    if !valid_address(address) {
        return Err(crate::ops::op_failure(format!(
            "refusing to build setup commands for `{address}` — the routing address must be \
             a literal IP. Set `remote.publish_address` in .ws.json to the server's address."
        )));
    }

    let report = probe(site, runtime, domain, address, None);
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
        Platform::MacOs => apply_macos_gaps(site, domain, &report, yes),
        Platform::Linux => {
            guide_linux_gaps(domain, &report);
            Ok(())
        }
        Platform::Other => {
            crate::ui::note(&format!(
                "automated setup isn't available on this platform yet — wire \
                 `*.{domain}` → {address} and trust Caddy's root CA manually."
            ));
            Ok(())
        }
    }
}

/// Walk the macOS privileged gaps interactively (or unattended with `--yes`).
fn apply_macos_gaps(site: &Site, domain: &str, report: &Report, yes: bool) -> Result<()> {
    let mut steps: Vec<Step> = Vec::new();
    if !report.dns_ok {
        steps.push(dns_step_macos(domain, &report.address));
    }
    if !report.ca_ok {
        // Fetch + persist the root CA (to ws's own config dir, not $TMPDIR) so the
        // trust command has a concrete, non-clobberable file to point at. Fetched
        // from `site` — for an offload workspace that's the *server's* root, which
        // is the one signing the leaves this client will see.
        match crate::caddy::write_root_ca(site) {
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
/// at it rather than driving `sudo` from `ws`. The script's `--address` is exactly
/// the report's address, so name it: on an offload client the default (loopback)
/// would wire the wildcard to the wrong machine, and that's silent breakage.
fn guide_linux_gaps(domain: &str, report: &Report) {
    let mut needs = Vec::new();
    if !report.dns_ok {
        needs.push(format!("wildcard DNS (*.{domain} → {})", report.address));
    }
    if !report.ca_ok {
        needs.push("trust Caddy's root CA".to_string());
    }
    if report.port < 1024 && !report.port_ok {
        needs.push("allow rootless bind to the privileged port (sysctl)".to_string());
    }
    crate::ui::note(&format!("missing: {}", needs.join("; ")));
    crate::ui::nudge(
        "run the setup script for your distro — it applies the privileged bits transparently:",
    );
    crate::ui::command_block(&[format!(
        "server-setup/debian.sh {domain} --address {}{}",
        report.address,
        if report.port == crate::caddy::DEFAULT_HTTPS_PORT {
            String::new()
        } else {
            format!(" --port {}", report.port)
        }
    )]);
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
        let step = dns_step_macos("myapp.test", "127.0.0.1");
        let joined = step.commands.join("\n");
        assert!(joined.contains("address=/myapp.test/127.0.0.1"), "{joined}");
        assert!(joined.contains("/etc/resolver/myapp.test"), "{joined}");
    }

    #[test]
    fn macos_dns_step_points_the_wildcard_at_an_offload_server() {
        // The whole point of piece B: for a remote workspace the wildcard must
        // answer with the *server's* address, while the resolver entry still
        // points at dnsmasq on loopback (dnsmasq is what serves that answer).
        let step = dns_step_macos("myapp.test", "100.64.1.2");
        let joined = step.commands.join("\n");
        assert!(joined.contains("address=/myapp.test/100.64.1.2"), "{joined}");
        assert!(!joined.contains("address=/myapp.test/127.0.0.1"), "{joined}");
        assert!(joined.contains("nameserver 127.0.0.1"), "{joined}");
    }

    #[test]
    fn macos_dns_step_replaces_an_existing_wildcard_instead_of_stacking_one() {
        // Re-running setup (a local↔remote flip, or a changed publish_address) must
        // not leave two `address=` lines for one domain: dnsmasq answers with both,
        // so a stale loopback entry would keep winning half the time.
        let step = dns_step_macos("myapp.test", "100.64.1.2");
        let joined = step.commands.join("\n");
        assert!(joined.contains("sed -i.ws-bak"), "must delete before appending: {joined}");
        // The delete pattern is anchored and dot-escaped, so `myapp.test` can't
        // also match `myappXtest` or a domain that merely contains it.
        assert!(joined.contains(r"/^address=\/myapp\.test\//d"), "{joined}");
        let del = joined.find("sed -i.ws-bak").unwrap();
        let add = joined.find("echo 'address=/myapp.test/100.64.1.2'").unwrap();
        assert!(del < add, "the delete must precede the append: {joined}");
    }

    #[test]
    fn valid_address_requires_a_literal_ip() {
        assert!(valid_address("127.0.0.1"));
        assert!(valid_address("100.64.1.2"));
        assert!(valid_address("::1"));
        // A name isn't a usable dnsmasq answer, however harmless it looks.
        assert!(!valid_address("srv"));
        assert!(!valid_address(""));
        // And a shell metacharacter can never reach a consented `sudo` line.
        assert!(!valid_address("127.0.0.1; rm -rf /"));
        assert!(!valid_address("$(whoami)"));
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
        let mut r = Report {
            port: 8443,
            address: "127.0.0.1".to_string(),
            dns_ok: true,
            ca_ok: true,
            port_ok: true,
        };
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
