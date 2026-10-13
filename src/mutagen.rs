//! Thin wrapper over the `mutagen` CLI; the `ws-…` session naming is a documented
//! stable contract. Binary resolved through `WSX_MUTAGEN` so tests can substitute a fake.

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::workspace;

/// Mutagen binary to invoke; overridable via `WSX_MUTAGEN` for tests.
fn mutagen_bin() -> String {
    std::env::var("WSX_MUTAGEN").unwrap_or_else(|_| "mutagen".to_string())
}

/// Cheap PATH check so `wsx init --remote` fails early instead of at `wsx new`.
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

/// Canonical sync-session name; a stable contract users may target with `mutagen sync pause wsx-…`.
pub fn session_name(branch: &str, repo: &str) -> String {
    format!("wsx-{}-{}", workspace::sanitize_branch(branch), repo)
}

/// Forwarding-session name, keyed by port and distinct from the sync name so
/// operations on one kind never touch the other.
pub fn forward_session_name(branch: &str, repo: &str, port: u16) -> String {
    format!("wsx-fwd-{}-{repo}-{port}", workspace::sanitize_branch(branch))
}

/// Forward server-side `localhost:<port>` to the same address on this machine so
/// tools that hardcode loopback keep working; idempotent on name collisions.
pub fn forward_create(session: &str, connection: &str, port: u16) -> Result<()> {
    let (local, remote) = forward_endpoints(connection, port);
    let out = Command::new(mutagen_bin())
        .args(["forward", "create", "--name", session, &local, &remote])
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("spawn {}", mutagen_bin()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        // Idempotent only on name collision, not local-bind "in use": swallowing the
        // latter would silently point the client at whichever process holds the port.
        if err.contains("already exists") {
            return Ok(());
        }
        bail!("mutagen forward create: {}", first_line(&err));
    }
    Ok(())
}

/// Same `localhost:<port>` on both ends so the address means the same thing wherever the stack runs.
fn forward_endpoints(connection: &str, port: u16) -> (String, String) {
    (format!("tcp:localhost:{port}"), format!("{connection}:tcp:localhost:{port}"))
}

/// Terminate a forwarding session. Best-effort: an absent session is success.
pub fn forward_terminate(session: &str) -> Result<()> {
    let out = Command::new(mutagen_bin())
        .args(["forward", "terminate", session])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("spawn {}", mutagen_bin()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("did not match")
            || err.contains("unable to locate")
            || err.contains("no sessions")
        {
            return Ok(());
        }
        bail!("mutagen forward terminate: {}", first_line(&err));
    }
    Ok(())
}

/// Sync ignores for a worktree: configured `worktree_excludes` plus per-host dep
/// artifacts. Gating on git tracking (not local presence) avoids a 200MB round-trip
/// where a two-way sync would drag a server-side `composer install` back over SSH.
pub fn sync_ignores(configured: &[String], worktree: &Path) -> Vec<String> {
    let mut ignores = configured.to_vec();
    // A linked worktree's `.git` is a FILE holding `gitdir: <client path>` — `--ignore-vcs`
    // matches only VCS *directories*, so without this the server receives a pointer into
    // a filesystem that isn't there and any git-shelling tool (e.g. `npm ci`) aborts.
    ignores.push("/.git".to_string());
    for dep in ["vendor", "node_modules"] {
        if !crate::git::is_tracked(worktree, dep) {
            ignores.push(format!("/{dep}"));
        }
    }
    ignores
}

/// The `beta` endpoint mutagen syncs to; `remote_path` matches what `wsx exec` uses so both agree on layout.
fn beta_endpoint(connection: &str, remote_path: &str) -> String {
    format!("{connection}:{remote_path}")
}

/// Create the beta root's parent on the server; mutagen creates the sync root itself
/// but not the directories above it, and a missing parent leaves the session reporting
/// `Watching for changes` while nothing ever transfers.
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
        bail!(
            "could not create {parent} on {connection}: {}",
            first_line(&String::from_utf8_lossy(&out.stderr))
        );
    }
    Ok(())
}

/// Confirm the sync root exists — mutagen's own status keeps reporting `Watching for
/// changes` on a dead session, so check the outcome directly.
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
    // Mutagen's portable mode creates files 0600/0700, unreadable by non-root service
    // users inside a rootless container; 0644/0755 matches a plain `git checkout` so the
    // stack behaves the same locally and on the server.
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
        // Idempotent: a session with this name already exists → fine.
        if err.contains("already exists") || err.contains("in use") {
            return Ok(());
        }
        bail!("mutagen sync create: {}", first_line(&err));
    }
    Ok(())
}

/// Terminate a sync session; a missing session is not an error.
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

/// Flush a sync session; best-effort by contract — callers treat errors as "nothing
/// flushed" since the session or `mutagen` itself may not exist yet.
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
        assert_eq!(session_name("feat/foo", "api"), "wsx-feat-foo-api");
        assert_eq!(session_name("bug/nested/x", "web"), "wsx-bug-nested-x-web");
        assert_eq!(session_name("plain", "api"), "wsx-plain-api");
    }

    #[test]
    fn forward_session_name_is_per_port_and_distinct_from_sync() {
        assert_eq!(forward_session_name("feat/foo", "app", 20000), "wsx-fwd-feat-foo-app-20000");
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

    /// A git repo with `files` committed so `is_tracked` has something real to answer against.
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
        // `--ignore-vcs` matches VCS directories only; a linked worktree's `.git` is a file, so it needs explicit ignore.
        let repo = repo_with(&["README.md"]);
        assert!(sync_ignores(&[], repo.path()).contains(&"/.git".to_string()));
    }

    #[test]
    fn untracked_deps_are_ignored_even_when_absent_locally() {
        // Guards the regression where gating on local presence dragged a server-side `composer install` back over SSH.
        let repo = repo_with(&["README.md"]);
        let ignores = sync_ignores(&[], repo.path());
        assert!(ignores.contains(&"/vendor".to_string()));
        assert!(ignores.contains(&"/node_modules".to_string()));
    }

    #[test]
    fn tracked_vendor_is_committed_source_and_stays_on_the_wire() {
        // Go-style vendoring: `vendor/` is tracked source, not a per-host artifact.
        let repo = repo_with(&["vendor/pkg/lib.go"]);
        let ignores = sync_ignores(&[], repo.path());
        assert!(!ignores.contains(&"/vendor".to_string()));
        assert!(ignores.contains(&"/node_modules".to_string()));
    }

    #[test]
    fn configured_excludes_come_through_alongside_the_deps() {
        // `worktree_excludes` is the escape hatch for every other ecosystem's dep dir.
        let repo = repo_with(&["README.md"]);
        let ignores = sync_ignores(&["/.venv".to_string()], repo.path());
        assert_eq!(ignores[0], "/.venv");
        assert!(ignores.contains(&"/vendor".to_string()));
    }
}
