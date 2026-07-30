//! Per-service host-port allocation for raw-TCP services, persisted in
//! `.ws-meta/<branch>-<repo>.json`.
//!
//! Two properties matter:
//!
//! - **Stable** — a service keeps the same port across `ws up` runs, so
//!   connection strings (and, later, Caddy routes) don't churn. The recorded
//!   allocation is reused; only a genuinely new service is allocated.
//! - **Collision-free across parallel worktrees** — a freshly allocated port
//!   avoids every port already recorded across the whole `.ws-meta` dir, and is
//!   test-bound to confirm it's free right now. Concurrent `ws` processes are
//!   serialized by an `O_EXCL` lockfile so two simultaneous allocators can't
//!   hand out the same port.
//!
//! The record in `.ws-meta` *is* the reservation. Truly atomic hold-until-bind
//! (the socket kept open until the service binds it) is a routing-slice
//! refinement; here the port is only test-bound, so a non-`ws` process could in
//! principle steal it in the window before the stack comes up — acceptable for
//! the local-first path.

use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashSet};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config::ServiceConfig;
use crate::remote::Site;

/// Port scan window: above the common service defaults, below the ephemeral
/// range, with room for many parallel worktrees.
const RANGE: std::ops::Range<u16> = 20000..40000;

/// The `.ws-meta` dir under a workspace root (per-machine allocation state).
pub fn meta_dir(root: &Path) -> PathBuf {
    root.join(".ws-meta")
}

fn meta_file(root: &Path, branch_sanitized: &str, repo: &str) -> PathBuf {
    meta_dir(root).join(format!("{branch_sanitized}-{repo}.json"))
}

/// The generated compose override for a worktree, alongside its port record in
/// `.ws-meta`. Gitignored and per-worktree; regenerated on each `up`.
pub fn override_file(root: &Path, branch_sanitized: &str, repo: &str) -> PathBuf {
    meta_dir(root).join(format!("{branch_sanitized}-{repo}.override.yml"))
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Meta {
    /// service name → allocated host port
    #[serde(default)]
    ports: BTreeMap<String, u16>,
    /// The effective service set recorded at the last `up` (derived + declared),
    /// so read-only phases (`down`/`status`/`list`/`exec`) can reconstruct
    /// `WS_HOST_*`/`WS_PORT_*` for services that were auto-derived from compose
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

fn write_meta(path: &Path, meta: &Meta) -> Result<()> {
    crate::config::write_json_pretty(path, meta)
}

/// Load the persisted host-port map for a worktree without allocating. Empty
/// when nothing has been allocated yet. Used by read-only / teardown paths.
pub fn load(root: &Path, branch_sanitized: &str, repo: &str) -> BTreeMap<String, u16> {
    read_meta(&meta_file(root, branch_sanitized, repo)).ports
}

/// The effective service set recorded at the last `up`, or empty if none. Read
/// paths use it so a derived-but-undeclared service still resolves its host/port.
pub fn services(root: &Path, branch_sanitized: &str, repo: &str) -> BTreeMap<String, ServiceConfig> {
    read_meta(&meta_file(root, branch_sanitized, repo)).services
}

/// Persist the effective service set for a worktree (called at `up`, preserving
/// the recorded ports). Best-effort context on write failure.
pub fn record_services(
    root: &Path,
    branch_sanitized: &str,
    repo: &str,
    services: &BTreeMap<String, ServiceConfig>,
) -> Result<()> {
    let dir = meta_dir(root);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    // Under the same lock `resolve` takes: read → set services → write. Without
    // it, a concurrent `resolve` (another terminal) that allocated a port
    // between our read and write would be silently clobbered.
    let _lock = Lock::acquire(&dir);
    let path = meta_file(root, branch_sanitized, repo);
    let mut meta = read_meta(&path);
    meta.services = services.clone();
    write_meta(&path, &meta)
}

/// Ensure every service in `needs` has a host port at `site`, allocating (and
/// persisting) any that are missing while preserving existing allocations.
/// Returns the full persisted map (may include services beyond `needs`).
///
/// The record, the used-set scan, and the lock all stay **host-side** — every
/// `ws` invocation for a workspace runs on the host, so the host's `.ws-meta` is
/// the single source of truth for what this workspace reserved and the host lock
/// serializes concurrent `ws up`s correctly. Only the *freeness probe* follows
/// `site`: a candidate must be free on the machine that will actually publish it
/// (the server, for a remote workspace). Because the fast path skips allocation
/// when everything is already recorded, a steady-state remote `up` makes no probe
/// round trips at all.
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
    // Fast path: nothing to allocate → skip the lock entirely.
    let existing = read_meta(&path);
    if needs.iter().all(|s| existing.ports.contains_key(s)) {
        return Ok(existing.ports);
    }

    let _lock = Lock::acquire(&dir);
    // Re-read under the lock — another process may have allocated since our
    // fast-path read.
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
    write_meta(&path, &meta)?;
    Ok(meta.ports)
}

/// Drop the persisted allocation for a worktree (frees its ports for reuse) and
/// its generated compose override. Best-effort — called from teardown, where a
/// missing file is fine.
pub fn forget(root: &Path, branch_sanitized: &str, repo: &str) {
    let _ = std::fs::remove_file(meta_file(root, branch_sanitized, repo));
    let _ = std::fs::remove_file(override_file(root, branch_sanitized, repo));
}

/// Every port recorded across the whole `.ws-meta` dir — the set a new
/// allocation must avoid so parallel worktrees never collide.
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

/// First port in `RANGE` that isn't already recorded and is free at `site` right
/// now. The reservation is the `.ws-meta` record; the probe just confirms nothing
/// else currently holds the port on the machine that will publish it.
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

/// Whether `port` is free at `site`. Local: try to bind it (the listener is
/// dropped immediately). Remote: ask the server whether anything is listening on
/// it (`ss`), since the port must be free where it will actually be published.
/// Both are "no live holder right now"; the record is the durable reservation.
///
/// The remote probe echoes an explicit `free`/`busy` token rather than inferring
/// freeness from a pipeline exit code — a bare `grep` exit also fires (as "no
/// match") when `ss` is missing or the SSH errored, which would read as free and
/// hand out a possibly-taken port. Requiring a literal `free` means any failure
/// (no `ss`, unreachable) reads as **not free**, so `free_port` moves on / fails
/// loudly rather than colliding silently. (`ss` — iproute2 — is a server
/// prerequisite; without it every port reads busy and allocation bails clearly.)
pub fn port_free_at(site: &Site, port: u16) -> bool {
    match site {
        Site::Local => TcpListener::bind(("127.0.0.1", port)).is_ok(),
        Site::Remote { .. } => {
            // Match `:<port>` / `.<port>` at the end of the local-address column so
            // `:200` can't match `:2000`.
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

/// A best-effort inter-process lock over `.ws-meta` via an `O_EXCL` lockfile,
/// removed on drop. Allocation holds it for milliseconds, so contention that
/// lasts a full window means the holder almost certainly crashed — we then
/// *steal* the stale lock rather than stall forever or leave it wedged.
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
                        // Presumed-dead holder: remove the stale file and take it
                        // on the next iteration. Give a fresh window so we don't
                        // thrash a holder that legitimately re-acquired just now
                        // (if two waiters race, one wins `create_new`).
                        let _ = std::fs::remove_file(&path);
                        deadline = Instant::now() + LOCK_WINDOW;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                // Any other error (e.g. a read-only dir) → give up on locking
                // but still allocate; correctness degrades gracefully.
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

        // Second resolve reuses the recorded ports (stable across runs).
        let again = resolve(&Site::Local, root, "feat-x", "api", &["mysql".into()]).unwrap();
        assert_eq!(again["mysql"], mysql);

        // Meta file is persisted where load() expects it.
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
