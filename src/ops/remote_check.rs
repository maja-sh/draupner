//! `ws remote check [--host <h>] [-v]` — read-only verification of where a
//! workspace's stack lives. Never installs, mutates, or `sudo`s; it only
//! observes (locally + over your own SSH), so it's safe to run against a server
//! you're still setting up. It's the contract for "is this host ready to route".
//!
//! Two modes: with a `remote` block (or `--host`) it probes the offload server;
//! without one it reports the local routing readiness (the same probe `ws setup`
//! uses, read-only).

use anyhow::Result;

use crate::{config, probe, setup, workspace};

pub fn run(host: Option<&str>, verbose: bool) -> Result<()> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;

    // Resolve the target + where its worktrees live. An explicit `--host` may name
    // a *different* server than the configured remote, so it uses the default
    // worktree base rather than inheriting the configured `workspace_path` (which
    // describes the configured server). `config::load` already rejected an unsafe
    // configured connection; a `--host` value is fresh input, so validate it here.
    let (connection, workspace_path) = match host {
        Some(h) => (Some(h.to_string()), config::default_worktree_base(&ws.root)),
        None => match &cfg.remote {
            Some(r) => (Some(r.connection.clone()), r.worktree_base(&ws.root)),
            None => (None, String::new()),
        },
    };

    match connection {
        Some(conn) => {
            crate::remote::ensure_connection_safe(&conn)
                .map_err(|e| super::preflight(e.to_string()))?;
            check_remote(&conn, &workspace_path, verbose)
        }
        None => check_local(&cfg, verbose),
    }
}

/// Probe the offload server: SSH reachable + mutagen local (client side), then a
/// single read-only SSH script (mutagen / podman / Caddy / workspace dir).
fn check_remote(connection: &str, workspace_path: &str, verbose: bool) -> Result<()> {
    crate::ui::header("ws-remote-check", connection);

    // One SSH connection: `server_probes` returns `None` precisely when SSH
    // couldn't connect, so we derive the reachability line from that instead of
    // paying a second handshake just to test it.
    let server = probe::server_probes(connection, workspace_path);

    // Render the ssh verdict as a plain status line — we derived it from the
    // server-probe connection, so we don't show a separate `ssh … true` command
    // that wasn't the one that actually ran (the server script is shown instead).
    let ssh = probe::ssh_status(connection, server.is_some());
    crate::ui::status_line(ssh.ok, ssh.name, &ssh.detail);
    let mut all_ok = ssh.ok;

    let mg = probe::mutagen_local();
    render(&mg, verbose);
    all_ok &= mg.ok;

    match server {
        Some(probes) => {
            if verbose {
                crate::ui::command_block(&[format!("ssh {connection} {}", probe::server_script(workspace_path))]);
            }
            for p in &probes {
                render(p, verbose);
                all_ok &= p.ok;
            }
        }
        None => {
            crate::ui::skip("server", "skipped — SSH isn't reachable");
            all_ok = false;
        }
    }

    crate::ui::note(&format!(
        "worktrees resolve to {connection}:{workspace_path}/<task>/<repo>"
    ));

    if all_ok {
        crate::ui::summary_ok(&[
            "remote looks ready.".to_string(),
            "Trust the server's Caddy root CA + wire wildcard DNS via \
             `server-setup/debian.sh` on the server if you haven't."
                .to_string(),
        ]);
        Ok(())
    } else {
        Err(super::op_failure(
            "remote check found problems — see the failing lines above",
        ))
    }
}

/// Report local routing readiness (read-only). Reuses `ws setup`'s probe so the
/// two agree on "what ready means"; unlike `ws setup` it changes nothing.
fn check_local(cfg: &config::WsConfig, verbose: bool) -> Result<()> {
    crate::ui::header("ws-remote-check", "local");

    let Some(domain) = cfg.domain.as_deref() else {
        crate::ui::note(
            "no `domain` set — no local routing to verify. Add `domain` to .ws.json \
             to route worktrees, or `ws init --remote` to offload to a server.",
        );
        return Ok(());
    };

    let report = setup::probe(cfg.runtime(), domain, None);
    setup::render_report(&report, domain);
    if verbose {
        crate::ui::command_block(&[
            format!("resolve a `*.{domain}` name → 127.0.0.1 (wildcard DNS)"),
            "TLS handshake to Caddy validates against the system trust store (CA)".to_string(),
        ]);
    }

    if report.privileged_gaps() {
        crate::ui::nudge("`ws setup` — walk through fixing the gaps above");
        return Err(super::op_failure("local routing setup is incomplete"));
    }
    crate::ui::summary_ok(&[format!(
        "local routing ready — reach a worktree at https://<branch>.<svc>.{domain}"
    )]);
    Ok(())
}

/// Render one probe as a status line, plus its command under `-v`.
fn render(p: &probe::Probe, verbose: bool) {
    crate::ui::status_line(p.ok, p.name, &p.detail);
    if verbose {
        if let Some(cmd) = &p.cmd {
            crate::ui::command_block(&[cmd.clone()]);
        }
    }
}
