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
use std::path::Path;
use std::process::{Command, Stdio};

use crate::workspace;

fn systemd_run_bin() -> String {
    std::env::var("WS_SYSTEMD_RUN").unwrap_or_else(|_| "systemd-run".to_string())
}

fn systemctl_bin() -> String {
    std::env::var("WS_SYSTEMCTL").unwrap_or_else(|_| "systemctl".to_string())
}

/// Transient unit name for a worktree's host process: `ws-<sanitized-branch>-<repo>`.
/// Mirrors the mutagen session naming so the whole per-worktree namespace is
/// consistent and greppable.
pub fn unit_name(branch: &str, repo: &str) -> String {
    format!("ws-{}-{}", workspace::sanitize_branch(branch), repo)
}

/// Start `cmd` as a transient `--user` unit named `unit`, in `worktree`, with
/// the given env. Idempotent: if the unit is already active it's left alone (a
/// watcher reloads on file change — no need to bounce a running server).
pub fn up(unit: &str, worktree: &Path, cmd: &str, env: &[(String, String)]) -> Result<()> {
    if is_active(unit) {
        return Ok(());
    }
    // Clear any lingering failed state so the unit name is free to reuse.
    reset_failed(unit);

    let mut c = Command::new(systemd_run_bin());
    c.args(["--user", "--quiet"])
        .arg(format!("--unit={unit}"))
        .arg(format!("--working-directory={}", worktree.display()))
        .args(["--property", "Restart=on-failure"]);
    for (k, v) in env {
        c.arg(format!("--setenv={k}={v}"));
    }
    c.arg("--").args(["sh", "-c", cmd]);

    let out = c
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("spawn {}", systemd_run_bin()))?;
    if !out.status.success() {
        bail!("systemd-run --user: {}", first_line(&String::from_utf8_lossy(&out.stderr)));
    }
    Ok(())
}

/// Stop a host unit (and clear failed state). Best-effort: a unit that was never
/// started, or is already gone, is not an error.
pub fn down(unit: &str) -> Result<()> {
    let _ = Command::new(systemctl_bin())
        .args(["--user", "stop", unit])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    reset_failed(unit);
    Ok(())
}

/// Whether a host unit is currently running (`systemctl --user is-active`).
pub fn is_active(unit: &str) -> bool {
    Command::new(systemctl_bin())
        .args(["--user", "is-active", unit])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn reset_failed(unit: &str) {
    let _ = Command::new(systemctl_bin())
        .args(["--user", "reset-failed", unit])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
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
