//! Host setup for per-worktree routing — readiness probe + consented fixes.

use anyhow::{bail, Context, Result};
use std::process::Command;

use crate::remote::Site;

/// Decides how privileged bits are delivered (macOS → interactive prompt; Linux → the setup script).
enum Platform {
    MacOs,
    Linux,
    Other,
}

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
    pub port: u16,
    /// Carried on the report so every rendered line, generated command and probe agrees on one value.
    pub address: String,
    /// A wildcard pointing at the *wrong* machine is distinct from one that doesn't resolve at all.
    pub dns: crate::caddy::Dns,
    pub ca_ok: bool,
    /// Always true unless the port is privileged (`< 1024`) and the sysctl hasn't been lowered.
    pub port_ok: bool,
}

impl Report {
    /// The rootless Caddy bit isn't counted — `ws` fixes that itself without a prompt.
    pub fn privileged_gaps(&self) -> bool {
        !self.dns.is_ok() || !self.ca_ok || !self.port_ok
    }
}

/// Shared by `ws setup` and `ws remote check`'s local mode so the two never drift.
pub fn render_report(report: &Report, domain: &str) {
    // Show what the wildcard *does* answer when wrong; a bare cross reads as "not wired yet".
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

/// Non-mutating; the single owner of "what counts as ready" — `ws up` and `ws setup` both read the `Report`.
///
/// `known_dns` lets a caller that already resolved the wildcard pass the answer in to skip a second lookup.
pub fn probe(
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

/// Ask the question that matters — does a TLS handshake to Caddy validate against *this* machine's system store?
///
/// `--resolve` pins the handshake at `address` so it needs no DNS; on-demand TLS mints a leaf signed by the internal root.
fn ca_trusted(domain: &str, port: u16, address: &str) -> bool {
    let host = format!("ws-ca-probe.{domain}");
    Command::new("curl")
        // Short timeout: this runs on the `ws up` hot path — a wedged Caddy mustn't stall the up.
        .args(["-sS", "-o", "/dev/null", "-m", "4", "--resolve"])
        .arg(format!("{host}:{port}:{address}"))
        .arg(format!("https://{host}:{port}"))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A privileged step: the commands embed their own `sudo` so the user sees precisely what elevates.
struct Step {
    what: String,
    commands: Vec<String>,
}

/// Refuses shell metacharacters so a hand-edited `.ws.json` can't smuggle a command into a consented `sudo` line.
fn valid_domain(domain: &str) -> bool {
    !domain.is_empty()
        && domain.len() <= 253
        && domain.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

/// Must parse as a literal IP — dnsmasq's `address=/domain/<answer>` wants an address, and an IP is inert in a shell.
fn valid_address(address: &str) -> bool {
    address.parse::<std::net::IpAddr>().is_ok()
}

/// Wires `*.<domain>` → `address` via dnsmasq + `/etc/resolver/<domain>`.
///
/// The wildcard line is deleted before append so a re-run (local↔remote flip) doesn't stack a stale loopback entry
/// alongside the new one — dnsmasq answers with every matching `address=` line. `restart` (not `start`) reloads a
/// running dnsmasq so the edited config actually takes effect. The install is guarded because a bare `brew install`
/// on a machine that has dnsmasq triggers a full Homebrew auto-update in the middle of the consent walk.
fn dns_step_macos(domain: &str, address: &str) -> Step {
    let conf = "\"$(brew --prefix)/etc/dnsmasq.conf\"";
    // Dots are regex metacharacters; domain is `[a-z0-9.-]`-validated so escaping is the only quoting needed.
    let pattern = domain.replace('.', "\\.");
    Step {
        what: format!(
            "resolve `*.{domain}` → {address} (dnsmasq + /etc/resolver; \
             installs dnsmasq only if missing, without a Homebrew auto-update)"
        ),
        commands: vec![
            "brew list dnsmasq >/dev/null 2>&1 || HOMEBREW_NO_AUTO_UPDATE=1 brew install dnsmasq"
                .to_string(),
            format!("sed -i.ws-bak '/^address=\\/{pattern}\\//d' {conf}"),
            format!("echo 'address=/{domain}/{address}' >> {conf}"),
            "sudo brew services restart dnsmasq".to_string(),
            "sudo mkdir -p /etc/resolver".to_string(),
            format!("echo 'nameserver 127.0.0.1' | sudo tee /etc/resolver/{domain}"),
        ],
    }
}

/// `pem_path` is single-quoted because it's an unvalidated filesystem path later run via `sh -c`.
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

/// Single-quote for `sh -c` interpolation; embedded single quotes become `'\''`.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Ensure the rootless infra directly at `site`, then walk each privileged gap per the host's delivery model.
pub fn run(
    site: &Site,
    domain: Option<&str>,
    runtime: &str,
    yes: bool,
    address: &str,
    publish_addr: &str,
) -> Result<()> {
    crate::ui::header("ws-setup", domain.unwrap_or("(no domain)"));

    if site.is_remote() {
        crate::ui::note(&format!(
            "this workspace offloads to {address} — readying Caddy there, then this \
             machine's DNS + CA trust so the routes are reachable from here."
        ));
    }

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
            crate::caddy::port_suffix(report.port)
        )]);
        return Ok(());
    }

    // A Linux box that runs the stacks defers to its versioned script; a Linux client has none, so gets the walk.
    match plat {
        Platform::MacOs => walk_gaps(macos_steps(site, domain, &report), yes),
        Platform::Linux if site.is_remote() => {
            walk_gaps(linux_client_steps(site, domain, &report), yes)
        }
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

/// The macOS privileged gaps as consentable steps.
fn macos_steps(site: &Site, domain: &str, report: &Report) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::new();
    if !report.dns.is_ok() {
        steps.push(dns_step_macos(domain, &report.address));
    }
    if !report.ca_ok {
        if let Some(path) = fetch_root_ca(site) {
            steps.push(ca_step_macos(&path));
        }
    }
    if report.port < 1024 && !report.port_ok {
        crate::ui::note(&format!(
            "binding :{} on macOS needs a privileged listener — not automated yet; \
             use the default :8443 or run Caddy privileged.",
            report.port
        ));
    }
    steps
}

/// Persists to `ws`'s config dir (not `$TMPDIR`) so the trust command has a non-clobberable file to point at.
fn fetch_root_ca(site: &Site) -> Option<String> {
    match crate::caddy::write_root_ca(site) {
        Ok(path) => Some(path.to_string_lossy().into_owned()),
        Err(e) => {
            let whose = if site.is_remote() { "the server's Caddy" } else { "Caddy" };
            crate::ui::note(&format!(
                "couldn't fetch Caddy's root CA to set up trust ({e:#}) — is {whose} running?"
            ));
            None
        }
    }
}

/// One step of the setup walk, callable on its own by a local↔remote flip that just invalidated the wildcard.
///
/// Returns whether the commands ran, so the caller can fall back to the stale-DNS note. `offloading` decides
/// the Linux branch — a client gets the interactive treatment; a Linux box that runs the stacks is not offered
/// anything here (it defers to `server-setup/<distro>.sh`).
pub fn offer_dns_step(domain: &str, address: &str, offloading: bool) -> Result<bool> {
    if !crate::ui::interactive() {
        return Ok(false);
    }
    // A failed address derivation hands us the server's *name*, which `valid_address` rejects.
    if !valid_domain(domain) || !valid_address(address) {
        return Ok(false);
    }
    let step = match platform() {
        Platform::MacOs => Some(dns_step_macos(domain, address)),
        Platform::Linux if offloading => dns_step_linux(domain, address),
        Platform::Linux | Platform::Other => None,
    };
    let Some(step) = step else { return Ok(false) };
    crate::ui::note("the wildcard's correct answer just changed — this re-points it:");
    match apply_step(&step, false)? {
        Outcome::Ran => {
            crate::ui::note(&format!("`*.{domain}` now answers {address}"));
            Ok(true)
        }
        Outcome::Skipped | Outcome::Printed => Ok(false),
    }
}

/// Walk a set of privileged gaps interactively (or unattended with `--yes`).
fn walk_gaps(steps: Vec<Step>, yes: bool) -> Result<()> {
    let mut printed = false;
    for step in &steps {
        printed |= matches!(apply_step(step, yes)?, Outcome::Printed);
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

/// `Printed` (non-interactive bail) is distinct from `Skipped` (user read and declined) so callers can act.
enum Outcome {
    Ran,
    Skipped,
    Printed,
}

/// `--yes` runs it; otherwise prompt `[R]un/[S]kip/?`.
fn apply_step(step: &Step, yes: bool) -> Result<Outcome> {
    // Count what actually escalates; the DNS step is six commands but only three `sudo`, so "one" would mislead consent.
    let privileged = step.commands.iter().filter(|c| c.contains("sudo ")).count();
    let detail = match privileged {
        0 => format!("{} command(s), none privileged", step.commands.len()),
        1 => format!("{} command(s), 1 privileged", step.commands.len()),
        n => format!("{} command(s), {n} privileged", step.commands.len()),
    };
    crate::ui::status_line(false, &step.what, &detail);
    if yes {
        run_commands(&step.commands)?;
        return Ok(Outcome::Ran);
    }
    loop {
        let Some(ans) = crate::ui::ask("[R]un / [S]kip / show-me? (r/s/?)") else {
            // Non-TTY: print and bail rather than hang on a sudo prompt.
            crate::ui::command_block(&step.commands);
            return Ok(Outcome::Printed);
        };
        match ans.to_lowercase().as_str() {
            "r" | "run" | "y" | "yes" | "" => {
                run_commands(&step.commands)?;
                return Ok(Outcome::Ran);
            }
            "s" | "skip" | "n" | "no" => return Ok(Outcome::Skipped),
            "?" | "show" | "show-me" => crate::ui::command_block(&step.commands),
            _ => {}
        }
    }
}

/// Inherits the terminal so `sudo` prompts directly — `ws` never reads the password.
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

/// On a stacks-running Linux box, defer to `server-setup/<distro>.sh` rather than driving `sudo` ourselves.
fn guide_linux_gaps(domain: &str, report: &Report) {
    let mut needs = Vec::new();
    if !report.dns.is_ok() {
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

/// Resolver step runs only when the mechanism is detected; otherwise print both forms rather than guess with `sudo`.
fn linux_client_steps(site: &Site, domain: &str, report: &Report) -> Vec<Step> {
    let mut steps = Vec::new();
    if !report.dns.is_ok() {
        match dns_step_linux(domain, &report.address) {
            Some(step) => steps.push(step),
            None => {
                crate::ui::note(
                    "couldn't tell which resolver this machine uses, so the wildcard isn't \
                     offered as a step — wire one of these by hand:",
                );
                crate::ui::command_block(&linux_client_commands(report, domain));
            }
        }
    }
    if !report.ca_ok {
        if let Some(path) = fetch_root_ca(site) {
            match ca_step_linux(&path) {
                Some(step) => steps.push(step),
                None => crate::ui::note(&format!(
                    "no system trust-store tool found (`update-ca-certificates` / \
                     `update-ca-trust`) — trust this file by your distro's method: {path}"
                )),
            }
        }
    }
    steps
}

/// `None` when neither systemd-resolved nor dnsmasq is identifiable, leaving the caller to print instead of guess.
fn dns_step_linux(domain: &str, address: &str) -> Option<Step> {
    if resolv_conf_is_systemd() {
        let iface = default_route_iface()?;
        return Some(Step {
            // resolvectl is runtime-scoped — say so rather than implying permanence.
            what: format!(
                "resolve `*.{domain}` → {address} via systemd-resolved on `{iface}` \
                 (runtime-scoped — re-run after a reboot, or set it in your network config)"
            ),
            commands: vec![
                format!("sudo resolvectl dns {iface} {address}"),
                format!("sudo resolvectl domain {iface} '~{domain}'"),
            ],
        });
    }
    if which("dnsmasq").is_some() {
        // A non-loopback resolver means lookups bypass dnsmasq; say so rather than reporting success on a no-op.
        let asks_dnsmasq = system_resolver_is_local();
        let caveat = if asks_dnsmasq {
            ""
        } else {
            " — NOTE: this machine's resolver is not local, so you must also point \
             /etc/resolv.conf at 127.0.0.1 for this to take effect"
        };
        return Some(Step {
            what: format!("resolve `*.{domain}` → {address} (dnsmasq drop-in){caveat}"),
            commands: vec![
                format!(
                    "echo 'address=/{domain}/{address}' | sudo tee /etc/dnsmasq.d/ws-{domain}.conf"
                ),
                "sudo systemctl restart dnsmasq".to_string(),
            ],
        });
    }
    None
}

/// Unreadable `resolv.conf` reads as "not local" so the step carries its caveat instead of silently promising a fix.
fn system_resolver_is_local() -> bool {
    let Ok(text) = std::fs::read_to_string("/etc/resolv.conf") else {
        return false;
    };
    text.lines()
        .filter_map(|l| l.strip_prefix("nameserver "))
        .filter_map(|a| a.trim().parse::<std::net::IpAddr>().ok())
        .any(|ip| ip.is_loopback())
}

/// `None` when neither `update-ca-certificates` nor `update-ca-trust` is present.
fn ca_step_linux(pem_path: &str) -> Option<Step> {
    let (dir, update) = if which("update-ca-certificates").is_some() {
        ("/usr/local/share/ca-certificates", "update-ca-certificates")
    } else if which("update-ca-trust").is_some() {
        ("/etc/pki/ca-trust/source/anchors", "update-ca-trust")
    } else {
        return None;
    };
    Some(Step {
        what: "trust Caddy's root CA in the system store".to_string(),
        commands: vec![
            format!("sudo cp {} {dir}/ws-caddy-root.crt", sh_quote(pem_path)),
            format!("sudo {update}"),
        ],
    })
}

/// Same signal `server-setup/debian.sh` uses to pick its resolver guidance.
fn resolv_conf_is_systemd() -> bool {
    std::fs::read_link("/etc/resolv.conf")
        .map(|t| t.to_string_lossy().contains("systemd"))
        .unwrap_or(false)
}

/// `resolvectl` needs a concrete link name, so read the default route's device.
fn default_route_iface() -> Option<String> {
    let out = Command::new("ip").args(["-o", "route", "show", "default"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_route_iface(&String::from_utf8_lossy(&out.stdout))
}

fn parse_route_iface(text: &str) -> Option<String> {
    let mut toks = text.split_whitespace();
    while let Some(t) = toks.next() {
        if t == "dev" {
            return toks.next().map(str::to_string).filter(|s| !s.is_empty());
        }
    }
    None
}

/// Searches `/usr/sbin` and `/sbin` too — `update-ca-certificates` and `dnsmasq` live there on Debian, off user `PATH`.
fn which(bin: &str) -> Option<()> {
    Command::new("sh")
        .arg("-c")
        .arg(format!("PATH=\"$PATH:/usr/sbin:/sbin\" command -v {bin}"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()
        .filter(|s| s.success())
        .map(|_| ())
}

/// Pure, so it's testable without a network.
fn linux_client_commands(report: &Report, domain: &str) -> Vec<String> {
    if report.dns.is_ok() {
        return Vec::new();
    }
    // One line per entry: `command_block` indents each, so an embedded newline breaks block alignment.
    let addr = &report.address;
    vec![
        "# wildcard DNS — dnsmasq:".to_string(),
        format!("echo 'address=/{domain}/{addr}' | sudo tee /etc/dnsmasq.d/ws-{domain}.conf"),
        "sudo systemctl restart dnsmasq".to_string(),
        "# …or systemd-resolved, routing just this domain:".to_string(),
        format!("sudo resolvectl dns    <iface> {addr}"),
        format!("sudo resolvectl domain <iface> '~{domain}'"),
    ]
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
        // `start` no-ops on an already-running dnsmasq, leaving the edited config unread.
        assert!(joined.contains("services restart dnsmasq"), "{joined}");
        assert!(!joined.contains("services start dnsmasq"), "{joined}");
    }

    #[test]
    fn macos_dns_step_points_the_wildcard_at_an_offload_server() {
        let step = dns_step_macos("myapp.test", "100.64.1.2");
        let joined = step.commands.join("\n");
        assert!(joined.contains("address=/myapp.test/100.64.1.2"), "{joined}");
        assert!(!joined.contains("address=/myapp.test/127.0.0.1"), "{joined}");
        assert!(joined.contains("nameserver 127.0.0.1"), "{joined}");
    }

    #[test]
    fn macos_dns_step_replaces_an_existing_wildcard_instead_of_stacking_one() {
        // dnsmasq answers with every matching `address=` line — a stale loopback would win half the time.
        let step = dns_step_macos("myapp.test", "100.64.1.2");
        let joined = step.commands.join("\n");
        assert!(joined.contains("sed -i.ws-bak"), "must delete before appending: {joined}");
        // Anchored and dot-escaped so `myapp.test` can't match `myappXtest`.
        assert!(joined.contains(r"/^address=\/myapp\.test\//d"), "{joined}");
        let del = joined.find("sed -i.ws-bak").unwrap();
        let add = joined.find("echo 'address=/myapp.test/100.64.1.2'").unwrap();
        assert!(del < add, "the delete must precede the append: {joined}");
    }

    #[test]
    fn linux_client_guidance_never_points_at_the_server_script() {
        // A client must not be told to run `server-setup/debian.sh` — that provisions a stacks-running box.
        let report = Report {
            port: 8443,
            address: "100.64.1.2".to_string(),
            dns: crate::caddy::Dns::Unresolved,
            ca_ok: true, // skip the CA branch: it would try to reach a Caddy
            port_ok: true,
        };
        let cmds = linux_client_commands(&report, "myapp.test");
        let joined = cmds.join("\n");
        assert!(!joined.contains("server-setup"), "client guidance named the server script: {joined}");
        assert!(!joined.contains("podman"), "{joined}");
        assert!(joined.contains("address=/myapp.test/100.64.1.2"), "{joined}");
        assert!(joined.contains("resolvectl"), "both resolver forms are offered: {joined}");
    }

    #[test]
    fn route_iface_is_read_from_the_dev_token() {
        assert_eq!(
            parse_route_iface("default via 192.168.1.1 dev enp1s0 proto dhcp metric 100 \n")
                .as_deref(),
            Some("enp1s0")
        );
        assert_eq!(parse_route_iface(""), None);
        assert_eq!(parse_route_iface("default via 10.0.0.1 proto static"), None);
    }

    #[test]
    fn valid_address_requires_a_literal_ip() {
        assert!(valid_address("127.0.0.1"));
        assert!(valid_address("100.64.1.2"));
        assert!(valid_address("::1"));
        assert!(!valid_address("srv"));
        assert!(!valid_address(""));
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
            dns: crate::caddy::Dns::Correct,
            ca_ok: true,
            port_ok: true,
        };
        assert!(!r.privileged_gaps(), "caddy_up is rootless, not a privileged gap");
        r.dns = crate::caddy::Dns::Unresolved;
        assert!(r.privileged_gaps());
        // A wildcard pointing at the wrong machine used to read as ready on remote workspaces.
        r.dns = crate::caddy::Dns::Wrong {
            got: "100.97.195.105".to_string(),
            expected: Some("127.0.0.1".to_string()),
        };
        assert!(r.privileged_gaps());
    }

    #[test]
    fn macos_dns_step_does_not_reinstall_dnsmasq_it_already_has() {
        // A bare `brew install` triggers an auto-update mid-walk; `brew list` does not.
        let step = dns_step_macos("myapp.test", "127.0.0.1");
        let install = &step.commands[0];
        assert!(install.starts_with("brew list dnsmasq"), "install must be guarded: {install}");
        assert!(install.contains("|| HOMEBREW_NO_AUTO_UPDATE=1 brew install dnsmasq"), "{install}");
        assert!(step.what.contains("only if missing"), "{}", step.what);
        assert!(step.what.contains("auto-update"), "{}", step.what);
    }

    #[test]
    fn sh_quote_neutralizes_metacharacters() {
        assert_eq!(sh_quote("/tmp/x.crt"), "'/tmp/x.crt'");
        assert_eq!(sh_quote("/t;rm -rf/x"), "'/t;rm -rf/x'");
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn apply_step_prints_and_bails_when_stdin_is_not_a_tty() {
        // Under `cargo test` stdin isn't a terminal — must never block on a sudo prompt.
        let step = ca_step_macos("/tmp/x.crt");
        assert!(
            matches!(apply_step(&step, false).unwrap(), Outcome::Printed),
            "non-interactive step must print (bail), not run"
        );
    }

    #[test]
    fn dns_step_is_not_offered_without_a_tty() {
        // A scripted flip must not hang on a prompt it can't finish; under `cargo test` stdin is the real non-TTY.
        assert!(!offer_dns_step("myapp.test", "100.64.1.2", true).unwrap());
        assert!(!offer_dns_step("myapp.test", "127.0.0.1", false).unwrap());
    }

    #[test]
    fn dns_step_is_not_offered_for_values_that_cant_be_shell_safe() {
        // The flip must not let a hand-edited `.ws.json` smuggle anything into a consented `sudo` line.
        assert!(!offer_dns_step("myapp.test; rm -rf /", "127.0.0.1", true).unwrap());
        assert!(!offer_dns_step("myapp.test", "the server `srv`", true).unwrap());
    }
}
