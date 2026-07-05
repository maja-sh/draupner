//! Thin wrapper over the `mutagen` CLI. For now only the session-name
//! convention and a best-effort flush are needed — `ws exec` flushes a
//! worktree's sync before running remotely so the server sees the latest edits.
//! Session create/terminate lands with the `up`/`down`/`rm` lifecycle work.
//!
//! The binary is resolved through `WS_MUTAGEN` (default `mutagen`) so tests can
//! substitute a fake.

use anyhow::{bail, Context, Result};
use std::process::{Command, Stdio};

use crate::workspace;

/// Mutagen binary to invoke. Overridable via `WS_MUTAGEN` for tests.
fn mutagen_bin() -> String {
    std::env::var("WS_MUTAGEN").unwrap_or_else(|_| "mutagen".to_string())
}

/// Canonical sync-session name for a worktree: `ws-<sanitized-branch>-<repo>`.
/// This is the documented escape hatch — a user can `mutagen sync pause ws-…`
/// directly — so the naming is a stable contract.
pub fn session_name(branch: &str, repo: &str) -> String {
    format!("ws-{}-{}", workspace::sanitize_branch(branch), repo)
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
}
