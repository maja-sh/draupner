//! Host setup for per-worktree routing — readiness probe + consented fixes.
//!
//! `ws` splits the host bits routing needs by privilege and **never runs `sudo`
//! silently**: the rootless ones (`ws-net`, the Caddy container) it just does;
//! the privileged ones (wildcard DNS, trusting Caddy's root CA, the
//! `ip_unprivileged_port_start` sysctl for an opt-in `:443`) are always shown
//! and consented.
//!
//! How the privileged bits are delivered differs by host *role*:
//!
//! - **macOS** (the workspace owner's machine): an interactive walk —
//!   `[R]un / [S]kip / show-me`, the command shelled with its own `sudo`
//!   inheriting the terminal, so `ws` never reads the password.
//! - **A Linux box that runs the stacks**: the bits live in a versioned,
//!   readable `server-setup/<distro>.sh` the user runs themselves — the script
//!   *is* the transparency, so `ws` reports gaps and points at it.
//! - **A Linux client of an offload server**: the script does *not* apply — it
//!   provisions a machine to *run* stacks, none of which a client needs to
//!   resolve a hostname. Only the two client-side bits are offered, and they get
//!   the same interactive walk macOS does.
//!
//! Non-interactive (stdin not a TTY): print the commands and bail — never hang on
//! a `sudo` password prompt. `--yes` runs the macOS steps unattended.

use anyhow::{bail, Context, Result};
use std::process::Command;

use crate::remote::Site;

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

/// A privileged step. The commands embed their own `sudo` so the user sees
/// precisely what elevates.
struct Step {
    what: String,
    commands: Vec<String>,
}

/// Refuses anything outside `[a-z0-9.-]` so a hand-edited `.ws.json` can't smuggle
/// a command into a consented `sudo` line.
fn valid_domain(domain: &str) -> bool {
    !domain.is_empty()
        && domain.len() <= 253
        && domain.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

/// The other externally-sourced value reaching a generated `sudo` line (from
/// `remote.publish_address` in a possibly-cloned `.ws.json`, or from `ssh -G`).
/// Requiring a literal IP is the real format requirement and not merely a safety
/// net — dnsmasq's `address=/domain/<answer>` wants an address, not a name — and
/// an IP is also inert in a shell.
fn valid_address(address: &str) -> bool {
    address.parse::<std::net::IpAddr>().is_ok()
}

/// Wire `*.<domain>` → `address` on macOS. `address` is the wildcard's *answer*
/// (loopback locally, the offload server otherwise); the resolver entry always
/// points at dnsmasq itself on loopback, which is what serves that answer.
///
/// The wildcard is replaced, not appended: dnsmasq answers with *every* matching
/// `address=` line, so re-running this after a local↔remote flip would leave a
/// stale entry still handing out the old address. `sed -i.ws-bak` takes its suffix
/// attached, the one form both BSD and GNU sed accept.
///
/// `restart`, not `start`: `start` no-ops on an already-running dnsmasq, leaving
/// the just-edited config unread and the old answer live.
///
/// The install is guarded because a bare `brew install` auto-updates Homebrew and
/// prints a wall of unrelated formulae — in the middle of a walk whose whole point
/// is that the user reads each privileged command before approving it.
fn dns_step_macos(domain: &str, address: &str) -> Step {
    let conf = "\"$(brew --prefix)/etc/dnsmasq.conf\"";
    // Dots are regex metacharacters; the domain is `[a-z0-9.-]`-validated, so
    // escaping them is the only quoting the pattern needs.
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

/// `pem_path` is quoted because it's a filesystem path, not a validated hostname,
/// and the whole line is later run via `sh -c`.
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

/// Single-quote for `sh -c` (embedded quotes become `'\''`).
///
/// Deliberately *always* wraps, unlike [`crate::remote::shell_quote`], which
/// passes safe tokens through bare: these lines are shown for consent and then run
/// under `sudo`, so the argument boundary must be unambiguous whatever the path
/// turns out to be.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Run `ws setup`. Any `--port` / `--admin-port` opt-in is already persisted by
/// `ops::setup`, so the probe and bring-up here read the new values back.
///
/// The rootless Caddy is ensured **at the site** — on the server over SSH for an
/// offload workspace. Nothing in `server-setup/<distro>.sh` starts it, and without
/// it the CA fetch below has nothing to fetch from. What is *not* started for a
/// remote workspace is a second, local Caddy, which would only fight for `:443`.
/// The privileged client-side bits run either way: they're what makes the server's
/// routes reachable and trusted from *this* machine.
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

    render_report(&report, domain);

    if !report.privileged_gaps() {
        crate::ui::summary_ok(&[format!(
            "routing is ready — reach a worktree at https://<branch>.<svc>.{domain}{}",
            port_suffix(report.port)
        )]);
        return Ok(());
    }

    // Split by host *role*, not platform: a Linux box that runs the stacks has a
    // versioned script that owns these commands and defers to it, while a Linux
    // *client* has none, so print-only would be strictly worse UX than macOS for
    // identical transparency.
    match std::env::consts::OS {
        "macos" => walk_gaps(macos_steps(site, domain, &report), yes),
        "linux" if site.is_remote() => walk_gaps(linux_client_steps(site, domain, &report), yes),
        "linux" => {
            guide_linux_gaps(domain, &report);
            Ok(())
        }
        _ => {
            crate::ui::note(&format!(
                "automated setup isn't available on this platform yet — wire \
                 `*.{domain}` → {address} and trust Caddy's root CA manually."
            ));
            Ok(())
        }
    }
}

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

/// Persisted to `ws`'s own config dir, not `$TMPDIR`, so the trust command has a
/// non-clobberable file to point at. Fetched from `site`: for an offload workspace
/// that's the **server's** root, the one signing the leaves this client will see.
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

/// Offer the wildcard-DNS step on its own — what a local↔remote flip calls once it
/// has invalidated the wildcard's answer (`ops::remote_set`). Returns whether the
/// commands ran, so the caller can otherwise fall back to saying what's now stale.
/// `offloading` is the state *after* the flip, which decides the Linux branch.
///
/// Two of the three macOS commands only rewrite a user-owned file and need no
/// privilege, but restarting dnsmasq to make them take effect does — so the whole
/// step stays behind one keypress rather than being split into a silent half and a
/// consented one. Nothing is offered without a TTY: the prompt couldn't be
/// answered, and a scripted flip must keep printing its stale-DNS note.
pub fn offer_dns_step(domain: &str, address: &str, offloading: bool) -> Result<bool> {
    if !crate::ui::interactive() {
        return Ok(false);
    }
    // The same guards `run` applies before building any consented command line. A
    // derivation that failed hands us the server's *name* rather than an address,
    // which `valid_address` rejects — so the caller falls back to the note.
    if !valid_domain(domain) || !valid_address(address) {
        return Ok(false);
    }
    let step = match std::env::consts::OS {
        "macos" => Some(dns_step_macos(domain, address)),
        "linux" if offloading => dns_step_linux(domain, address),
        _ => None,
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

/// `Printed` is the non-interactive bail (commands shown, nothing run), distinct
/// from `Skipped`, which is a user who read them and declined.
enum Outcome {
    Ran,
    Skipped,
    Printed,
}

fn apply_step(step: &Step, yes: bool) -> Result<Outcome> {
    // Count what the step actually escalates rather than asserting "one": the DNS
    // step is six commands, three of them `sudo`, and promising one password
    // prompt then giving three makes the consent worth less than it looks.
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
            _ => {} // re-prompt
        }
    }
}

/// Inherits the terminal so `sudo` prompts directly — `ws` never reads the
/// password. Stops at the first failure.
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

/// For a Linux box that **runs the stacks**, the privileged bits are shown rather
/// than driven: `server-setup/<distro>.sh` owns them alongside the runtime
/// prerequisites, and the script *is* the transparency. `--address` is named
/// explicitly because the script defaults to loopback.
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

/// The resolver hop is the one genuinely environment-specific bit (systemd-resolved
/// vs dnsmasq vs a plain `resolv.conf`), and writing the wrong one either silently
/// no-ops or disturbs system DNS — so it becomes a runnable step only when the
/// mechanism is detected, and otherwise falls back to printing both forms.
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
                crate::ui::command_block(&linux_client_commands(domain, &report.address));
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

/// systemd-resolved is detected the same way `server-setup/debian.sh` detects it,
/// and needs a concrete interface, so the default route's device is read. `None`
/// when neither resolver is identifiable, leaving the caller to print not guess.
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
    if which("dnsmasq") {
        // A non-loopback `nameserver` means the system never asks dnsmasq, so the
        // drop-in would be written and lookups would still go straight past it.
        // Say so rather than reporting success on a no-op; `ws` won't rewrite
        // `resolv.conf` itself, which NetworkManager or dhcp usually owns.
        let asks_dnsmasq = system_resolver_is_local();
        let caveat = if asks_dnsmasq {
            ""
        } else {
            " — NOTE: this machine's resolver is not local, so you must also point \
             /etc/resolv.conf at 127.0.0.1 for this to take effect"
        };
        return Some(Step {
            what: format!("resolve `*.{domain}` → {address} (dnsmasq drop-in){caveat}"),
            commands: dnsmasq_dropin_commands(domain, address),
        });
    }
    None
}

fn dnsmasq_dropin_commands(domain: &str, address: &str) -> Vec<String> {
    vec![
        format!("echo 'address=/{domain}/{address}' | sudo tee /etc/dnsmasq.d/ws-{domain}.conf"),
        "sudo systemctl restart dnsmasq".to_string(),
    ]
}

/// Whether queries reach a local resolver at all, which decides whether a dnsmasq
/// drop-in can take effect. An unreadable `resolv.conf` reads as "not local": that
/// makes the step carry its caveat rather than silently promising a fix.
fn system_resolver_is_local() -> bool {
    let Ok(text) = std::fs::read_to_string("/etc/resolv.conf") else {
        return false;
    };
    text.lines()
        .filter_map(|l| l.strip_prefix("nameserver "))
        .filter_map(|a| a.trim().parse::<std::net::IpAddr>().ok())
        .any(|ip| ip.is_loopback())
}

fn ca_step_linux(pem_path: &str) -> Option<Step> {
    let (dir, update) = if which("update-ca-certificates") {
        ("/usr/local/share/ca-certificates", "update-ca-certificates")
    } else if which("update-ca-trust") {
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

/// The same signal `server-setup/debian.sh` uses to pick its resolver guidance.
fn resolv_conf_is_systemd() -> bool {
    std::fs::read_link("/etc/resolv.conf")
        .map(|t| t.to_string_lossy().contains("systemd"))
        .unwrap_or(false)
}

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

/// Searches `/usr/sbin` and `/sbin` as well as `PATH`: both tools this looks for
/// live in `/usr/sbin` on Debian, which isn't on a normal user's `PATH`, so a plain
/// `command -v` reports them missing on the exact systems that have them — silently
/// downgrading a runnable step into a printed hint.
fn which(bin: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("PATH=\"$PATH:/usr/sbin:/sbin\" command -v {bin}"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The fallback [`linux_client_steps`] prints when the resolver can't be
/// identified: both forms, rather than guessing at one.
fn linux_client_commands(domain: &str, address: &str) -> Vec<String> {
    // One line per entry: `command_block` indents each element, so an embedded
    // newline would render flush-left and break the block's alignment.
    let mut cmds = vec!["# wildcard DNS — dnsmasq:".to_string()];
    cmds.extend(dnsmasq_dropin_commands(domain, address));
    cmds.extend([
        "# …or systemd-resolved, routing just this domain:".to_string(),
        // Padded to align the two `<iface>` columns inside `command_block`.
        format!("sudo resolvectl dns    <iface> {address}"),
        format!("sudo resolvectl domain <iface> '~{domain}'"),
    ]);
    cmds
}

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
        // Must `restart`, not `start`: `start` no-ops on an already-running
        // dnsmasq, leaving the edited config unread and the old answer live.
        assert!(joined.contains("services restart dnsmasq"), "{joined}");
        assert!(!joined.contains("services start dnsmasq"), "{joined}");
    }

    #[test]
    fn macos_dns_step_points_the_wildcard_at_an_offload_server() {
        // For a remote workspace the wildcard must answer with the *server's*
        // address, while the resolver entry still points at dnsmasq on loopback
        // (dnsmasq is what serves that answer).
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
    fn linux_client_guidance_never_points_at_the_server_script() {
        // A Linux client of an offload server must not be told to run
        // `server-setup/debian.sh` — that provisions a machine to *run* stacks
        // (podman, compose, mutagen, the podman socket) to fix a DNS entry.
        // Asserted on the command text because that's what the user acts on.
        let cmds = linux_client_commands("myapp.test", "100.64.1.2");
        let joined = cmds.join("\n");
        assert!(!joined.contains("server-setup"), "client guidance named the server script: {joined}");
        assert!(!joined.contains("podman"), "{joined}");
        // It must still wire the wildcard at the *server's* address.
        assert!(joined.contains("address=/myapp.test/100.64.1.2"), "{joined}");
        assert!(joined.contains("resolvectl"), "both resolver forms are offered: {joined}");
    }

    #[test]
    fn route_iface_is_read_from_the_dev_token() {
        // Real `ip -o route show default` shape. `resolvectl` needs a concrete
        // link name, so a misparse here would generate an unrunnable command.
        assert_eq!(
            parse_route_iface("default via 192.168.1.1 dev enp1s0 proto dhcp metric 100 \n")
                .as_deref(),
            Some("enp1s0")
        );
        // A machine with no default route yields nothing rather than a bad guess.
        assert_eq!(parse_route_iface(""), None);
        assert_eq!(parse_route_iface("default via 10.0.0.1 proto static"), None);
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
    fn macos_dns_step_does_not_reinstall_dnsmasq_it_already_has() {
        // A bare `brew install` on a machine that has dnsmasq triggers a full
        // Homebrew auto-update and prints a wall of unrelated formulae — in the
        // middle of a walk whose point is that the user reads each privileged
        // command before approving it. `brew list` does not auto-update.
        let step = dns_step_macos("myapp.test", "127.0.0.1");
        let install = &step.commands[0];
        assert!(install.starts_with("brew list dnsmasq"), "install must be guarded: {install}");
        assert!(install.contains("|| HOMEBREW_NO_AUTO_UPDATE=1 brew install dnsmasq"), "{install}");
        // And the step text says so, since that's what consent is given against.
        assert!(step.what.contains("only if missing"), "{}", step.what);
        assert!(step.what.contains("auto-update"), "{}", step.what);
    }

    #[test]
    fn sh_quote_neutralizes_metacharacters() {
        assert_eq!(sh_quote("/tmp/x.crt"), "'/tmp/x.crt'");
        assert_eq!(sh_quote("/t;rm -rf/x"), "'/t;rm -rf/x'");
        // An embedded single quote is escaped, not a break-out.
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn which_answers_yes_or_no_for_a_binary() {
        // `sh` is what `which` itself shells out to, so it is present by
        // construction; the absent case must be a clean `false`, not a panic.
        assert!(which("sh"));
        assert!(!which("ws-no-such-binary-exists-here"));
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
        assert!(
            matches!(apply_step(&step, false).unwrap(), Outcome::Printed),
            "non-interactive step must print (bail), not run"
        );
    }

    #[test]
    fn dns_step_is_not_offered_without_a_tty() {
        // The flip's offer must be a no-op for a scripted run: no prompt it can't
        // finish, no command block, and — the reason this matters — no hang. The
        // caller then prints the stale-DNS note exactly as it always did.
        assert!(!offer_dns_step("myapp.test", "100.64.1.2", true).unwrap());
        assert!(!offer_dns_step("myapp.test", "127.0.0.1", false).unwrap());
    }

    #[test]
    fn dns_step_is_not_offered_for_values_that_cant_be_shell_safe() {
        // Same guards `run` applies: a hand-edited `.ws.json` must not be able to
        // smuggle anything into a consented `sudo` line by way of the flip, and a
        // failed address derivation hands over the server's *name*, which is not a
        // usable dnsmasq answer.
        assert!(!offer_dns_step("myapp.test; rm -rf /", "127.0.0.1", true).unwrap());
        assert!(!offer_dns_step("myapp.test", "the server `srv`", true).unwrap());
    }
}
