//! Thin wrapper over the `mutagen` CLI. Sync sessions are created when a
//! worktree is made (`ws new`) and terminated when it's removed (`ws rm` /
//! `ws clean`); `ws exec` flushes before a remote run. Session naming is a
//! stable contract (the documented escape hatch).
//!
//! The binary is resolved through `WS_MUTAGEN` (default `mutagen`) so tests can
//! substitute a fake.
//!
//! NOTE: the exact mutagen flag names/behavior here are built from docs, not
//! yet verified against a live `mutagen` — verify when wiring a real server
//! (spec-remote's standing "the runtime wins" caveat).

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::workspace;

/// Mutagen binary to invoke. Overridable via `WS_MUTAGEN` for tests.
fn mutagen_bin() -> String {
    std::env::var("WS_MUTAGEN").unwrap_or_else(|_| "mutagen".to_string())
}

/// Is `mutagen` available on PATH? Cheap client-side readiness check — a
/// remote workspace needs it locally to sync, so `ws init --remote` verifies it
/// up front rather than letting `ws new` fail later.
pub fn installed() -> bool {
    Command::new(mutagen_bin())
        .arg("version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Canonical sync-session name for a worktree: `ws-<sanitized-branch>-<repo>`.
/// This is the documented escape hatch — a user can `mutagen sync pause ws-…`
/// directly — so the naming is a stable contract.
pub fn session_name(branch: &str, repo: &str) -> String {
    format!("ws-{}-{}", workspace::sanitize_branch(branch), repo)
}

/// Sync ignores for a worktree: the repo's configured `worktree_excludes` plus
/// the per-host dep artifacts (`vendor/`, `node_modules/`) present on disk.
/// Disk presence is the ground truth at both call sites — `ws new` creates the
/// sync after deps land, `ws remote set` backfills an existing worktree — and
/// deps are arch-specific: they belong installed on each side, not on the wire.
/// The one home for "what stays off the sync"; a drifted copy would ship a
/// node_modules-scale tree over SSH.
///
/// A dep dir that is git-TRACKED is committed source (Go-style vendoring), not
/// a per-host artifact — it must reach the server, so it is never ignored.
pub fn sync_ignores(configured: &[String], worktree: &Path) -> Vec<String> {
    let mut ignores = configured.to_vec();
    for dep in ["vendor", "node_modules"] {
        if worktree.join(dep).exists() && !crate::git::is_tracked(worktree, dep) {
            ignores.push(format!("/{dep}"));
        }
    }
    ignores
}

/// The `beta` (remote) endpoint mutagen syncs to: `<connection>:<remote_path>`.
/// `connection` is the SSH host; `remote_path` is resolved the same way `ws
/// exec` resolves its `cd` target, so sync and exec always agree on the layout.
fn beta_endpoint(connection: &str, remote_path: &str) -> String {
    format!("{connection}:{remote_path}")
}

/// Create a two-way sync between the local worktree (`local`) and the server
/// (`<connection>:<remote_path>`). `ignores` keeps per-worktree artifacts
/// (node_modules, vendor) off the wire; `--ignore-vcs` drops the worktree's
/// `.git` pointer (meaningless on the server). Idempotent: an existing session
/// of the same name is treated as success.
pub fn create(
    session: &str,
    local: &Path,
    connection: &str,
    remote_path: &str,
    ignores: &[String],
) -> Result<()> {
    let beta = beta_endpoint(connection, remote_path);
    let mut cmd = Command::new(mutagen_bin());
    cmd.args(["sync", "create", "--name", session, "--ignore-vcs"]);
    for ig in ignores {
        cmd.arg(format!("--ignore={ig}"));
    }
    cmd.arg(local).arg(&beta);
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("spawn {}", mutagen_bin()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        // Idempotent: a session with this name already exists → fine.
        if err.contains("already exists") || err.contains("in use") {
            return Ok(());
        }
        bail!("mutagen sync create: {}", first_line(&err));
    }
    Ok(())
}

/// Terminate a sync session. Best-effort: a session that's already gone (never
/// created, or removed out of band) is not an error.
pub fn terminate(session: &str) -> Result<()> {
    let out = Command::new(mutagen_bin())
        .args(["sync", "terminate", session])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("spawn {}", mutagen_bin()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        // No such session → already gone, treat as success.
        if err.contains("did not match")
            || err.contains("unable to locate")
            || err.contains("no sessions")
        {
            return Ok(());
        }
        bail!("mutagen sync terminate: {}", first_line(&err));
    }
    Ok(())
}

fn first_line(s: &str) -> &str {
    s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("failed").trim()
}

/// Flush a sync session so the far side has the latest edits. Best-effort by
/// contract: the caller treats any error as "nothing flushed" rather than a
/// hard failure, because the session may not exist yet (sync lifecycle isn't
/// wired into `up` yet) or `mutagen` may not be installed. Errors carry a
/// terse reason for a dim note; they never abort the op.
pub fn flush(session: &str) -> Result<()> {
    let out = Command::new(mutagen_bin())
        .args(["sync", "flush", session])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("spawn {}", mutagen_bin()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let reason = err.lines().find(|l| !l.trim().is_empty()).unwrap_or("flush failed");
        bail!("{}", reason.trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_name_sanitizes_branch() {
        assert_eq!(session_name("feat/foo", "api"), "ws-feat-foo-api");
        assert_eq!(session_name("bug/nested/x", "web"), "ws-bug-nested-x-web");
        assert_eq!(session_name("plain", "api"), "ws-plain-api");
    }

    #[test]
    fn beta_endpoint_is_scp_style() {
        assert_eq!(
            beta_endpoint("srv", "workspace/feat-foo/api"),
            "srv:workspace/feat-foo/api"
        );
    }
}
