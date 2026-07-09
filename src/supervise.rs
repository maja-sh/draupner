//! `systemd --user` supervision for host-process `up` scripts.
//!
//! A host repo's `up` is a long-running dev server (`cargo watch -x run`,
//! `vite`, `php artisan serve`). Running it via `sh -c` and waiting would block
//! `ws up` forever, so instead `ws` starts it as a transient `systemd-run
//! --user` unit. That buys, for free: crash-restart (`Restart=on-failure`),
//! journald logs (`journalctl --user -u ws-…`), a clean stop, liveness for
//! `ws list`, and survival past `ws` exiting. Needs `loginctl enable-linger`
//! once (a setup-doc line).
//!
//! The binaries are resolved through `WS_SYSTEMD_RUN` / `WS_SYSTEMCTL` so tests
//! can substitute fakes that record their argv.

use anyhow::{bail, Context, Result};
use std::process::{Command, Stdio};

use crate::remote::Site;
use crate::workspace;

fn systemd_run_bin() -> String {
    std::env::var("WS_SYSTEMD_RUN").unwrap_or_else(|_| "systemd-run".to_string())
}

fn systemctl_bin() -> String {
    std::env::var("WS_SYSTEMCTL").unwrap_or_else(|_| "systemctl".to_string())
}

/// Build the command that runs a `systemd-run`/`systemctl` invocation (`argv`) at
/// `site`. Local: spawn the binary directly, honoring the `WS_SYSTEMD_RUN` /
/// `WS_SYSTEMCTL` test seams (argv[0]) — behavior unchanged. Remote: run it on the
/// server over SSH, first exporting `XDG_RUNTIME_DIR`. A non-login SSH command has
/// no D-Bus session address, so `systemctl --user` would otherwise fail with
/// "Failed to connect to bus: No medium found"; linger keeps the user manager
/// alive (a server-setup step) and this points the client at `/run/user/<uid>`.
fn systemd_command(site: &Site, argv: &[String]) -> Command {
    match site {
        Site::Local => {
            let mut c = Command::new(&argv[0]);
            c.args(&argv[1..]);
            c
        }
        Site::Remote { .. } => {
            let quoted: Vec<String> = argv.iter().map(|a| crate::remote::shell_quote(a)).collect();
            site.shell(&format!("export XDG_RUNTIME_DIR=/run/user/$(id -u); {}", quoted.join(" ")))
        }
    }
}

/// Transient unit name for a worktree's host process: `ws-<sanitized-branch>-<repo>`.
/// Mirrors the mutagen session naming so the whole per-worktree namespace is
/// consistent and greppable.
pub fn unit_name(branch: &str, repo: &str) -> String {
    format!("ws-{}-{}", workspace::sanitize_branch(branch), repo)
}

/// Start `cmd` as a transient `--user` unit named `unit`, in `worktree`, with
/// the given env. Idempotent: if the unit is already active it's left running (a
/// watcher reloads on file change — no need to bounce a healthy server) and
/// `Ok(false)` is returned so the caller can say so rather than imply a fresh
/// start. `Ok(true)` when it actually launched. A changed `up` command/env only
/// takes effect after `ws down` — the returned flag lets the UI hint that.
pub fn up(site: &Site, unit: &str, worktree: &str, cmd: &str, env: &[(String, String)]) -> Result<bool> {
    if is_active(site, unit) {
        return Ok(false);
    }
    // Clear any lingering failed state so the unit name is free to reuse.
    reset_failed(site, unit);

    let mut argv: Vec<String> = vec![
        systemd_run_bin(),
        "--user".into(),
        "--quiet".into(),
        format!("--unit={unit}"),
        format!("--working-directory={worktree}"),
        "--property".into(),
        "Restart=on-failure".into(),
    ];
    for (k, v) in env {
        argv.push(format!("--setenv={k}={v}"));
    }
    argv.extend(["--".into(), "sh".into(), "-c".into(), cmd.to_string()]);

    let out = systemd_command(site, &argv)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("spawn {}", systemd_run_bin()))?;
    if !out.status.success() {
        bail!("systemd-run --user: {}", first_line(&String::from_utf8_lossy(&out.stderr)));
    }
    Ok(true)
}

/// Stop a host unit (and clear failed state). A unit that was never started, or
/// is already gone, is fine — but a stop that leaves the unit *still running* is
/// an error, so a caller (teardown) doesn't remove the worktree out from under a
/// live process.
pub fn down(site: &Site, unit: &str) -> Result<()> {
    let _ = systemd_command(site, &stanza("stop", unit))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    reset_failed(site, unit);
    // The `is_active` guard confirms the stop landed. Remote caveat: over a *flaky*
    // SSH link `is_active` can't reach the server and returns `false`, so `down`
    // reports success without confirming — teardown would then drop the worktree
    // while the server process may still run. Accepted (teardown is best-effort;
    // `up`'s idempotency needs the same `false` default, and a truly dead server is
    // handled by the `None`-site skip); a genuinely-up unit on a live link is caught.
    if is_active(site, unit) {
        bail!("unit {unit} is still active after stop");
    }
    Ok(())
}

/// Whether a host unit is currently running (`systemctl --user is-active`) at `site`.
pub fn is_active(site: &Site, unit: &str) -> bool {
    systemd_command(site, &stanza("is-active", unit))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn reset_failed(site: &Site, unit: &str) {
    let _ = systemd_command(site, &stanza("reset-failed", unit))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// The argv for a one-verb `systemctl --user <verb> <unit>` invocation.
fn stanza(verb: &str, unit: &str) -> [String; 4] {
    [systemctl_bin(), "--user".into(), verb.into(), unit.into()]
}

fn first_line(s: &str) -> &str {
    s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_name_sanitizes_branch() {
        assert_eq!(unit_name("feat/foo", "api"), "ws-feat-foo-api");
        assert_eq!(unit_name("plain", "web"), "ws-plain-web");
    }
}
