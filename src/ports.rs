//! Per-service host-port allocation for raw-TCP services, persisted in
//! `.ws-meta/<branch>-<repo>.json`.
//!
//! The record *is* the reservation. A candidate is only test-bound, never held
//! open until the service binds it, so a non-`ws` process can in principle steal
//! one in the window before the stack comes up — accepted for the local-first
//! path; hold-until-bind is a routing-slice refinement.

use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashSet};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config::ServiceConfig;
use crate::remote::Site;

/// Above the common service defaults, below the ephemeral range, with room for
/// many parallel worktrees.
const RANGE: std::ops::Range<u16> = 20000..40000;

pub fn meta_dir(root: &Path) -> PathBuf {
    root.join(".ws-meta")
}

fn meta_file(root: &Path, branch_sanitized: &str, repo: &str) -> PathBuf {
    meta_dir(root).join(format!("{branch_sanitized}-{repo}.json"))
}

/// The generated compose override, kept alongside the port record because both
/// are per-worktree machine state that `forget` drops together.
pub fn override_file(root: &Path, branch_sanitized: &str, repo: &str) -> PathBuf {
    meta_dir(root).join(format!("{branch_sanitized}-{repo}.override.yml"))
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Meta {
    #[serde(default)]
    ports: BTreeMap<String, u16>,
    /// Recorded at `up` so the read-only phases (`down`/`status`/`list`/`exec`)
    /// can reconstruct `WS_HOST_*`/`WS_PORT_*` for services derived from compose
    /// and never declared. Empty for a workspace that predates this.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    services: BTreeMap<String, ServiceConfig>,
}

fn read_meta(path: &Path) -> Meta {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Read-only counterpart to `resolve`: never allocates, empty when nothing has
/// been allocated yet.
pub fn load(root: &Path, branch_sanitized: &str, repo: &str) -> BTreeMap<String, u16> {
    read_meta(&meta_file(root, branch_sanitized, repo)).ports
}

pub fn services(root: &Path, branch_sanitized: &str, repo: &str) -> BTreeMap<String, ServiceConfig> {
    read_meta(&meta_file(root, branch_sanitized, repo)).services
}

pub fn record_services(
    root: &Path,
    branch_sanitized: &str,
    repo: &str,
    services: &BTreeMap<String, ServiceConfig>,
) -> Result<()> {
    let dir = meta_dir(root);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    // Under the same lock `resolve` takes: without it, a concurrent `resolve` that
    // allocated a port between our read and write would be silently clobbered.
    let _lock = Lock::acquire(&dir);
    let path = meta_file(root, branch_sanitized, repo);
    let mut meta = read_meta(&path);
    meta.services = services.clone();
    crate::config::write_json_pretty(&path, &meta)
}

/// Ensure every service in `needs` has a host port at `site`. Returns the full
/// persisted map, which may include services beyond `needs`.
///
/// The record, the used-set scan and the lock stay **host-side** even for a
/// remote workspace: every `ws` invocation runs on the host, so that is the only
/// place that can serialize concurrent allocators. Only the freeness probe
/// follows `site`, to the machine that will actually publish the port.
pub fn resolve(
    site: &Site,
    root: &Path,
    branch_sanitized: &str,
    repo: &str,
    needs: &[String],
) -> Result<BTreeMap<String, u16>> {
    let dir = meta_dir(root);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;

    let path = meta_file(root, branch_sanitized, repo);
    // Nothing to allocate → skip the lock, and (remotely) every probe round trip.
    let existing = read_meta(&path);
    if needs.iter().all(|s| existing.ports.contains_key(s)) {
        return Ok(existing.ports);
    }

    let _lock = Lock::acquire(&dir);
    // Re-read: another process may have allocated since the unlocked read above.
    let mut meta = read_meta(&path);
    let mut used = used_ports(&dir);
    used.extend(meta.ports.values().copied());

    for svc in needs {
        if meta.ports.contains_key(svc) {
            continue;
        }
        let port = free_port(&used, site)?;
        used.insert(port);
        meta.ports.insert(svc.clone(), port);
    }
    crate::config::write_json_pretty(&path, &meta)?;
    Ok(meta.ports)
}

/// Best-effort — called from teardown, where a missing file is fine.
pub fn forget(root: &Path, branch_sanitized: &str, repo: &str) {
    let _ = std::fs::remove_file(meta_file(root, branch_sanitized, repo));
    let _ = std::fs::remove_file(override_file(root, branch_sanitized, repo));
}

/// Scans the whole `.ws-meta` dir, not just this worktree's record, so parallel
/// worktrees never collide.
fn used_ports(dir: &Path) -> HashSet<u16> {
    let mut set = HashSet::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.filter_map(std::result::Result::ok) {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("json") {
                set.extend(read_meta(&p).ports.values().copied());
            }
        }
    }
    set
}

fn free_port(used: &HashSet<u16>, site: &Site) -> Result<u16> {
    for port in RANGE {
        if used.contains(&port) {
            continue;
        }
        if port_free_at(site, port) {
            return Ok(port);
        }
    }
    anyhow::bail!("no free host port in {}..{}", RANGE.start, RANGE.end)
}

/// Both arms mean "no live holder right now"; the `.ws-meta` record is the
/// durable reservation.
///
/// The remote probe echoes a literal `free`/`busy` rather than reading a pipeline
/// exit code: a bare `grep` also exits non-zero when `ss` is missing or the SSH
/// failed, which would read as free and hand out a taken port. Demanding the word
/// `free` makes every failure read as busy instead.
pub fn port_free_at(site: &Site, port: u16) -> bool {
    match site {
        Site::Local => TcpListener::bind(("127.0.0.1", port)).is_ok(),
        Site::Remote { .. } => {
            // Anchored to the end of the local-address column so `:200` can't
            // match `:2000`.
            let script = format!(
                "command -v ss >/dev/null 2>&1 || exit 3; \
                 if ss -Hltn 2>/dev/null | awk '{{print $4}}' | grep -qE '[.:]{port}$'; \
                 then echo busy; else echo free; fi"
            );
            site.shell(&script)
                .output()
                .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "free")
                .unwrap_or(false)
        }
    }
}

/// A best-effort inter-process lock over `.ws-meta` via an `O_EXCL` lockfile.
/// Allocation holds it for milliseconds, so contention lasting a full window
/// means the holder almost certainly crashed — the stale lock is then *stolen*
/// rather than left to wedge every future `ws up`.
struct Lock {
    path: PathBuf,
    held: bool,
}

const LOCK_WINDOW: Duration = Duration::from_secs(5);

impl Lock {
    fn acquire(dir: &Path) -> Lock {
        let path = dir.join(".lock");
        let mut deadline = Instant::now() + LOCK_WINDOW;
        loop {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Lock { path, held: true },
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if Instant::now() >= deadline {
                        // Presumed-dead holder. The window resets so that two
                        // waiters racing here don't thrash a holder that
                        // legitimately re-acquired in between.
                        let _ = std::fs::remove_file(&path);
                        deadline = Instant::now() + LOCK_WINDOW;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                // Can't lock (e.g. a read-only dir) → allocate anyway rather than
                // fail; correctness degrades gracefully.
                Err(_) => return Lock { path, held: false },
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        if self.held {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_allocates_persists_and_reuses() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let first = resolve(&Site::Local, root, "feat-x", "api", &["mysql".into(), "redis".into()]).unwrap();
        assert_eq!(first.len(), 2);
        let mysql = first["mysql"];
        let redis = first["redis"];
        assert_ne!(mysql, redis);
        assert!((RANGE.start..RANGE.end).contains(&mysql));

        let again = resolve(&Site::Local, root, "feat-x", "api", &["mysql".into()]).unwrap();
        // A recorded port is reused, so connection strings stay stable across runs.
        assert_eq!(again["mysql"], mysql);

        assert_eq!(load(root, "feat-x", "api")["mysql"], mysql);
    }

    #[test]
    fn distinct_worktrees_get_distinct_ports() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let a = resolve(&Site::Local, root, "feat-a", "api", &["mysql".into()]).unwrap()["mysql"];
        let b = resolve(&Site::Local, root, "feat-b", "api", &["mysql".into()]).unwrap()["mysql"];
        assert_ne!(a, b, "parallel worktrees must not share a host port");
    }

    #[test]
    fn free_port_avoids_used_set() {
        let mut used = HashSet::new();
        let p1 = free_port(&used, &Site::Local).unwrap();
        used.insert(p1);
        let p2 = free_port(&used, &Site::Local).unwrap();
        assert_ne!(p1, p2);
    }

    #[test]
    fn free_port_bails_when_range_exhausted() {
        let used: HashSet<u16> = (RANGE.start..RANGE.end).collect();
        assert!(free_port(&used, &Site::Local).is_err(), "a fully-used range must fail, not loop");
    }

    #[test]
    fn forget_removes_the_allocation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        resolve(&Site::Local, root, "feat-x", "api", &["mysql".into()]).unwrap();
        assert!(!load(root, "feat-x", "api").is_empty());
        forget(root, "feat-x", "api");
        assert!(load(root, "feat-x", "api").is_empty());
    }
}
