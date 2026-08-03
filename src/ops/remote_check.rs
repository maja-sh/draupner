//! Strictly read-only: this never installs, mutates or `sudo`s, so it stays safe
//! to run against a server you are half-way through setting up.

use anyhow::Result;

use crate::{config, probe, workspace};

pub fn run(host: Option<&str>, verbose: bool) -> Result<()> {
    let ws = workspace::locate()?;
    let cfg = config::load(&ws.root)?;

    // An explicit `--host` may name a *different* server than the configured
    // remote, so it takes the default worktree base rather than inheriting a
    // `workspace_path` that describes the configured one.
    let target = match host {
        Some(h) => Some((h.to_string(), config::default_worktree_base(&ws.root))),
        None => cfg
            .remote
            .as_ref()
            .map(|r| (r.connection.clone(), r.worktree_base(&ws.root))),
    };

    match target {
        Some((conn, workspace_path)) => {
            // `config::load` vetted the configured connection; a `--host` is fresh
            // input headed for ssh argv and has to be refused here.
            crate::remote::ensure_connection_safe(&conn)
                .map_err(|e| super::preflight(e.to_string()))?;
            check_remote(&conn, &workspace_path, cfg.domain.as_deref(), verbose)
        }
        None => check_local(&cfg, verbose),
    }
}

fn check_remote(connection: &str, workspace_path: &str, domain: Option<&str>, verbose: bool) -> Result<()> {
    crate::ui::header("ws-remote-check", connection);

    // `server_probes` returns `None` precisely when SSH couldn't connect, so the
    // reachability verdict comes from it rather than a second handshake — which is
    // also why the ssh line shows no command: none was run on its own account.
    let server = probe::server_probes(connection, workspace_path, domain);
    let ssh = probe::ssh_status(connection, server.is_some());
    crate::ui::status_line(ssh.ok, ssh.name, &ssh.detail);
    let mut all_ok = ssh.ok;

    let mg = probe::mutagen_local();
    crate::ui::status_line(mg.ok, mg.name, &mg.detail);
    if verbose {
        crate::ui::command_block(&["mutagen version".to_string()]);
    }
    all_ok &= mg.ok;

    match server {
        Some(probes) => {
            if verbose {
                crate::ui::command_block(&[format!(
                    "ssh {connection} {}",
                    probe::server_script(workspace_path, domain)
                )]);
            }
            for p in &probes {
                crate::ui::status_line(p.ok, p.name, &p.detail);
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
        // The probes above cover the server's own DNS; the client's resolver and
        // trust store are a separate requirement, and go unchecked by this command.
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

/// Reuses `ws setup`'s probe so the two agree on what "ready" means; unlike
/// `ws setup` it changes nothing.
fn check_local(cfg: &config::WsConfig, verbose: bool) -> Result<()> {
    crate::ui::header("ws-remote-check", "local");

    let Some(domain) = cfg.domain.as_deref() else {
        crate::ui::note(
            "no `domain` set — no local routing to verify. Add `domain` to .ws.json \
             to route worktrees, or `ws init --remote` to offload to a server.",
        );
        return Ok(());
    };

    // The local half, so the site and the address are loopback by definition.
    let report = probe::readiness(&crate::remote::Site::Local, cfg.runtime(), domain, "127.0.0.1", None);
    probe::render_report(&report, domain);
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
        "local routing ready — reach a worktree at https://<branch>.<svc>.{domain}"
    )]);
    Ok(())
}

