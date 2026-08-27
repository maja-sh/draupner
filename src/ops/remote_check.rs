//! `ws remote check` — read-only verification of where a workspace's stack
//! lives, safe to run against a server you're still setting up.

use anyhow::Result;

use crate::{config, probe, setup, workspace};

pub fn run(host: Option<&str>, verbose: bool) -> Result<()> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;

    // `--host` may name a different server than the configured remote, so use the default base and revalidate below.
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
            check_remote(&conn, &workspace_path, cfg.runtime(), cfg.domain.as_deref(), verbose)
        }
        None => check_local(&cfg, verbose),
    }
}

/// Probe the offload server over one SSH connection: local mutagen plus a single read-only server script.
fn check_remote(
    connection: &str,
    workspace_path: &str,
    runtime: &str,
    domain: Option<&str>,
    verbose: bool,
) -> Result<()> {
    crate::ui::header("ws-remote-check", connection);

    // `server_probes` returns `None` iff SSH failed, so derive the ssh line from that instead of paying a second handshake.
    let server = probe::server_probes(connection, workspace_path, runtime, domain);

    // Status line rather than a command block: the server script below is what actually ran, not a separate `ssh … true`.
    let ssh = probe::ssh_status(connection, server.is_some());
    crate::ui::status_line(ssh.ok, ssh.name, &ssh.detail);
    let mut all_ok = ssh.ok;

    let mg = probe::mutagen_local();
    render(&mg, verbose);
    all_ok &= mg.ok;

    match server {
        Some(probes) => {
            if verbose {
                crate::ui::command_block(&[format!(
                    "ssh {connection} {}",
                    probe::server_script(workspace_path, runtime, domain)
                )]);
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
        // Server-side DNS is covered above; the client's resolver and CA trust are separate requirements worth calling out.
        let client_side = match domain {
            Some(d) => format!(
                "Still client-side: trust the server's Caddy root CA, and resolve *.{d} \
                 to the server from here (`ws setup`)."
            ),
            None => "Trust the server's Caddy root CA from this machine if you haven't.".to_string(),
        };
        crate::ui::summary_ok(&["remote looks ready.".to_string(), client_side]);
        Ok(())
    } else {
        Err(super::op_failure(
            "remote check found problems — see the failing lines above",
        ))
    }
}

/// Report local routing readiness by reusing `ws setup`'s probe so the two agree on "ready".
fn check_local(cfg: &config::WsConfig, verbose: bool) -> Result<()> {
    crate::ui::header("ws-remote-check", "local");

    let Some(domain) = cfg.domain.as_deref() else {
        crate::ui::note(
            "no `domain` set — no local routing to verify. Add `domain` to .ws.json \
             to route worktrees, or `ws init --remote` to offload to a server.",
        );
        return Ok(());
    };

    // Local half: site and address are loopback by definition.
    let report = setup::probe(&crate::remote::Site::Local, cfg.runtime(), domain, "127.0.0.1", None);
    setup::render_report(&report, domain);
    if verbose {
        crate::ui::command_block(&[
            format!("resolve a `*.{domain}` name → {} (wildcard DNS)", report.address),
            "TLS handshake to Caddy validates against the system trust store (CA)".to_string(),
        ]);
    }

    if report.privileged_gaps() {
        crate::ui::nudge("`ws setup` — walk through fixing the gaps above");
        return Err(super::op_failure("local routing setup is incomplete"));
    }
    crate::ui::summary_ok(&[format!(
        "local routing ready — reach a worktree at https://<branch>.<svc>.{domain}{}",
        crate::caddy::port_suffix(report.port)
    )]);
    Ok(())
}

fn render(p: &probe::Probe, verbose: bool) {
    crate::ui::status_line(p.ok, p.name, &p.detail);
    if verbose {
        if let Some(cmd) = &p.cmd {
            crate::ui::command_block(std::slice::from_ref(cmd));
        }
    }
}
