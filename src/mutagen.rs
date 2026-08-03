//! Thin wrapper over the `mutagen` CLI. See ARCHITECTURE.md for where sync and
//! forward sessions sit in the lifecycle.
//!
//! The flag names here were built from mutagen's docs and have not been verified
//! against a live `mutagen` — read a surprising failure as a spec bug first.

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::ui;
use crate::workspace;

fn mutagen_bin() -> String {
    std::env::var("WS_MUTAGEN").unwrap_or_else(|_| "mutagen".to_string())
}

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

/// Users are documented to drive these sessions directly (`mutagen sync pause
/// ws-…`), so the name is a stable contract.
pub fn session_name(branch: &str, repo: &str) -> String {
    workspace::ws_name(branch, repo)
}

/// Keyed by port because a repo can expose several, and kept distinct from the
/// sync name so listing or terminating one kind never touches the other.
pub fn forward_session_name(branch: &str, repo: &str, port: u16) -> String {
    format!("ws-fwd-{}-{repo}-{port}", workspace::sanitize_branch(branch))
}

pub fn forward_create(session: &str, connection: &str, port: u16) -> Result<()> {
    let (local, remote) = forward_endpoints(connection, port);
    let out = Command::new(mutagen_bin())
        .args(["forward", "create", "--name", session, &local, &remote])
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("spawn {}", mutagen_bin()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        // Only a session-name collision is idempotent success. Unlike sync's
        // `create`, a bind "in use" must NOT be swallowed here: it means another
        // process holds `localhost:<port>`, and reporting success would point the
        // user's client at that process.
        if err.contains("already exists") {
            return Ok(());
        }
        bail!("mutagen forward create: {}", ui::first_nonempty_line(&err).unwrap_or("failed"));
    }
    Ok(())
}

/// Deliberately the same `localhost:<port>` on both ends, so an address means the
/// identical thing wherever the stack runs.
fn forward_endpoints(connection: &str, port: u16) -> (String, String) {
    (format!("tcp:localhost:{port}"), format!("{connection}:tcp:localhost:{port}"))
}

/// Terminate every port forward recorded for a worktree, best-effort — the
/// callers (teardown, a server re-point, `ws down`) must not be blockable by a
/// dead session. Must run while the `.ws-meta` port record still exists: the
/// forwards are re-derived from it, so a record wiped first strands them.
pub fn terminate_forwards(root: &std::path::Path, branch: &str, sanitized: &str, repo: &str) {
    for (_, port) in crate::ports::load(root, sanitized, repo) {
        let _ = forward_terminate(&forward_session_name(branch, repo, port));
    }
}

/// Best-effort: an absent session is success.
pub fn forward_terminate(session: &str) -> Result<()> {
    terminate_session("forward", session)
}

/// The single home for "what stays off the sync" — a drifted copy would ship a
/// node_modules-scale tree over SSH.
///
/// Git tracking, not local presence, decides: a tracked `vendor/` is committed
/// source (Go-style vendoring) and must reach the server. Presence is the trap —
/// the sync is two-way, so a dep dir that exists only at the far end (a remote
/// `composer install`, or a fresh clone with nothing for `ws new` to copy) gets
/// dragged back over SSH. Ignoring a path that does not exist costs nothing.
pub fn sync_ignores(configured: &[String], worktree: &Path) -> Vec<String> {
    let mut ignores = configured.to_vec();
    // `--ignore-vcs` matches VCS *directories*, and a linked worktree's `.git` is a
    // file holding a client-side `gitdir:` path — so without this the server gets a
    // pointer into a filesystem that isn't there, and git then aborts in that
    // directory before doing any work (`npm ci`'s `ls-remote` dies where no `.git`
    // at all would have been fine).
    ignores.push("/.git".to_string());
    for dep in ["vendor", "node_modules"] {
        if !crate::git::is_tracked(worktree, dep) {
            ignores.push(format!("/{dep}"));
        }
    }
    ignores
}

fn beta_endpoint(connection: &str, remote_path: &str) -> String {
    format!("{connection}:{remote_path}")
}

/// Create the beta root's **parent** on the server: mutagen creates the sync root
/// itself but not the directories above it, and the per-task directory never
/// exists on a first `ws new`. Skipping this fails silently — the session still
/// reports `Watching for changes` while nothing transfers.
fn ensure_beta_parent(connection: &str, remote_path: &str) -> Result<()> {
    let Some((parent, _)) = remote_path.rsplit_once('/') else { return Ok(()) };
    if parent.is_empty() {
        return Ok(());
    }
    let script = format!("mkdir -p {}", crate::remote::shell_quote(parent));
    let out = crate::remote::ssh_command(connection, &script, false)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("spawn ssh to {connection}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!(
            "could not create {parent} on {connection}: {}",
            ui::first_nonempty_line(&err).unwrap_or("failed")
        );
    }
    Ok(())
}

/// Checks the outcome rather than mutagen's own status, which is not a usable
/// signal: a session whose transfer never completes still reports `Watching for
/// changes`, with the failure only in the human-readable `sync list --long`.
pub fn verify_beta(connection: &str, remote_path: &str) -> Result<()> {
    let script = format!("test -d {}", crate::remote::shell_quote(remote_path));
    let ok = crate::remote::ssh_command(connection, &script, false)
        .stdin(Stdio::null())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        bail!(
            "sync reported success but {remote_path} is missing on {connection} — \
             check `mutagen sync list --long`"
        );
    }
    Ok(())
}

/// Create a two-way sync between the local worktree and the server. Idempotent:
/// an existing session of the same name is treated as success.
pub fn create(
    session: &str,
    local: &Path,
    connection: &str,
    remote_path: &str,
    ignores: &[String],
) -> Result<()> {
    ensure_beta_parent(connection, remote_path)?;
    let beta = beta_endpoint(connection, remote_path);
    let mut cmd = Command::new(mutagen_bin());
    cmd.args(["sync", "create", "--name", session, "--ignore-vcs"]);
    // Mutagen's portable permission mode creates everything 0600/0700 — owner-only,
    // and the owner is the SSH user, which a rootless container maps to root. A
    // service running as www-data or nginx then cannot traverse into the code it
    // was pointed at (php-fpm reports `File not found.` for a file that is plainly
    // there). 0644/0755 is what a plain `git checkout` produces; the accepted cost
    // is that `copy`-seeded secrets land group/other-readable on the server, the
    // same exposure as any checkout of the repo on that box.
    cmd.args([
        "--default-file-mode=0644",
        "--default-directory-mode=0755",
    ]);
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
        if err.contains("already exists") || err.contains("in use") {
            return Ok(());
        }
        bail!("mutagen sync create: {}", ui::first_nonempty_line(&err).unwrap_or("failed"));
    }
    Ok(())
}

/// Best-effort: a session that's already gone (never created, or removed out of
/// band) is not an error.
pub fn terminate(session: &str) -> Result<()> {
    terminate_session("sync", session)
}

fn terminate_session(kind: &str, session: &str) -> Result<()> {
    let out = Command::new(mutagen_bin())
        .args([kind, "terminate", session])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("spawn {}", mutagen_bin()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        // No such session → already gone; teardown has to stay idempotent.
        if err.contains("did not match")
            || err.contains("unable to locate")
            || err.contains("no sessions")
        {
            return Ok(());
        }
        bail!("mutagen {kind} terminate: {}", ui::first_nonempty_line(&err).unwrap_or("failed"));
    }
    Ok(())
}

/// Best-effort by contract: callers treat any error as "nothing flushed" rather
/// than a failure, since the session may not exist yet or `mutagen` may not be
/// installed. The error text is only ever shown as a dim note.
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
        bail!("{}", ui::first_nonempty_line(&err).unwrap_or("flush failed"));
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
    fn forward_session_name_is_per_port_and_distinct_from_sync() {
        assert_eq!(forward_session_name("feat/foo", "app", 20000), "ws-fwd-feat-foo-app-20000");
        assert_ne!(
            forward_session_name("feat", "api", 20000),
            forward_session_name("feat", "api", 20001)
        );
        assert_ne!(forward_session_name("feat", "api", 20000), session_name("feat", "api"));
    }

    #[test]
    fn forward_endpoints_are_same_port_both_ends() {
        let (local, remote) = forward_endpoints("srv", 20000);
        assert_eq!(local, "tcp:localhost:20000");
        assert_eq!(remote, "srv:tcp:localhost:20000");
    }

    #[test]
    fn beta_endpoint_is_scp_style() {
        assert_eq!(
            beta_endpoint("srv", "workspace/feat-foo/api"),
            "srv:workspace/feat-foo/api"
        );
    }

    fn repo_with(files: &[&str]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(tmp.path())
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        for f in files {
            let p = tmp.path().join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "x").unwrap();
        }
        run(&["add", "-A"]);
        run(&["commit", "-qm", "init"]);
        tmp
    }

    #[test]
    fn the_worktree_git_pointer_never_reaches_the_server() {
        let repo = repo_with(&["README.md"]);
        assert!(sync_ignores(&[], repo.path()).contains(&"/.git".to_string()));
    }

    #[test]
    fn untracked_deps_are_ignored_even_when_absent_locally() {
        // The regression: gating on local presence let a two-way sync drag a
        // server-side `composer install` (~200MB) back to the client.
        let repo = repo_with(&["README.md"]);
        let ignores = sync_ignores(&[], repo.path());
        assert!(ignores.contains(&"/vendor".to_string()));
        assert!(ignores.contains(&"/node_modules".to_string()));
    }

    #[test]
    fn tracked_vendor_is_committed_source_and_stays_on_the_wire() {
        let repo = repo_with(&["vendor/pkg/lib.go"]);
        let ignores = sync_ignores(&[], repo.path());
        assert!(!ignores.contains(&"/vendor".to_string()));
        assert!(ignores.contains(&"/node_modules".to_string()));
    }

    #[test]
    fn configured_excludes_come_through_alongside_the_deps() {
        let repo = repo_with(&["README.md"]);
        let ignores = sync_ignores(&["/.venv".to_string()], repo.path());
        assert_eq!(ignores[0], "/.venv");
        assert!(ignores.contains(&"/vendor".to_string()));
    }
}
