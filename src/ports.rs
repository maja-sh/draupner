//! Per-service host-port allocation persisted in `.ws-meta/<branch>-<repo>.json`,
//! stable across `up` runs and collision-free across parallel worktrees.

use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashSet};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config::ServiceConfig;
use crate::remote::Site;

/// Above common service defaults, below the ephemeral range.
const RANGE: std::ops::Range<u16> = 20000..40000;

/// Width when `WS_PORT_BASE` relocates the window — smaller than the default
/// because a relocated window serves one workspace, not the whole machine.
const MOVED_WIDTH: u16 = 500;

/// Scan window for [`free_port`], honouring the `WS_PORT_BASE` seam that gives
/// concurrent test workspaces non-overlapping windows.
fn range() -> std::ops::Range<u16> {
    window_from(std::env::var("WS_PORT_BASE").ok().and_then(|v| v.trim().parse().ok()))
}

/// Pure core of [`range`] so the seam's rules are testable without mutating the
/// process environment.
fn window_from(base: Option<u16>) -> std::ops::Range<u16> {
    match base {
        // A base below 1024 would hand out privileged ports no rootless process can bind.
        Some(base) if base >= 1024 => base..base.saturating_add(MOVED_WIDTH),
        _ => RANGE,
    }
}

pub fn meta_dir(root: &Path) -> PathBuf {
    root.join(".ws-meta")
}

fn meta_file(root: &Path, branch_sanitized: &str, repo: &str) -> PathBuf {
    meta_dir(root).join(format!("{branch_sanitized}-{repo}.json"))
}

pub fn override_file(root: &Path, branch_sanitized: &str, repo: &str) -> PathBuf {
    meta_dir(root).join(format!("{branch_sanitized}-{repo}.override.yml"))
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Meta {
    #[serde(default)]
    ports: BTreeMap<String, u16>,
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

pub fn load(root: &Path, branch_sanitized: &str, repo: &str) -> BTreeMap<String, u16> {
    read_meta(&meta_file(root, branch_sanitized, repo)).ports
}

/// Effective service set from the last `up` so derived-but-undeclared services
/// still resolve their host/port on read paths.
pub fn services(root: &Path, branch_sanitized: &str, repo: &str) -> BTreeMap<String, ServiceConfig> {
    read_meta(&meta_file(root, branch_sanitized, repo)).services
}

/// Persist the effective service set for a worktree, preserving the recorded ports.
pub fn record_services(
    root: &Path,
    branch_sanitized: &str,
    repo: &str,
    services: &BTreeMap<String, ServiceConfig>,
) -> Result<()> {
    let dir = meta_dir(root);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    // Under `resolve`'s lock so a concurrent allocation between our read and write isn't clobbered.
    let _lock = Lock::acquire(&dir);
    let path = meta_file(root, branch_sanitized, repo);
    let mut meta = read_meta(&path);
    meta.services = services.clone();
    write_meta(&path, &meta)
}

/// Allocate any missing host ports for `needs` while preserving existing ones;
/// state and lock stay host-side, but the freeness probe follows `site`.
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
    // Fast path skips the lock entirely when nothing needs allocating.
    let existing = read_meta(&path);
    if needs.iter().all(|s| existing.ports.contains_key(s)) {
        return Ok(existing.ports);
    }

    let _lock = Lock::acquire(&dir);
    // Re-read under the lock; another process may have allocated since our fast-path read.
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

/// Best-effort drop; a missing file is fine at teardown.
pub fn forget(root: &Path, branch_sanitized: &str, repo: &str) {
    let _ = std::fs::remove_file(meta_file(root, branch_sanitized, repo));
    let _ = std::fs::remove_file(override_file(root, branch_sanitized, repo));
}

/// Every port recorded across `.ws-meta`, so parallel worktrees never collide.
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
    let window = range();
    for port in window.clone() {
        if used.contains(&port) {
            continue;
        }
        if port_free_at(site, port) {
            return Ok(port);
        }
    }
    anyhow::bail!("no free host port in {}..{}", window.start, window.end)
}

/// Remote probe requires a literal `free` token so an `ss`/SSH failure reads
/// as not-free instead of silently handing out a taken port.
pub fn port_free_at(site: &Site, port: u16) -> bool {
    match site {
        Site::Local => TcpListener::bind(("127.0.0.1", port)).is_ok(),
        Site::Remote { .. } => {
            // Anchor `:<port>` / `.<port>` at end of column so `:200` can't match `:2000`.
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

/// Best-effort `O_EXCL` lockfile over `.ws-meta`; a full-window wait presumes
/// the holder crashed and steals the stale lock rather than stall forever.
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
                        // Steal, then reset the deadline so a legitimate re-acquirer isn't thrashed.
                        let _ = std::fs::remove_file(&path);
                        deadline = Instant::now() + LOCK_WINDOW;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                // Other errors (e.g. read-only dir): give up on locking but still allocate.
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
    fn a_moved_window_is_disjoint_from_the_default() {
        let moved = window_from(Some(41000));
        assert_eq!(moved, 41000..41000 + MOVED_WIDTH);
        assert!(moved.start >= RANGE.end, "a moved window must clear the default: {moved:?}");

        let a = window_from(Some(41000));
        let b = window_from(Some(41000 + MOVED_WIDTH));
        assert!(a.end <= b.start, "{a:?} and {b:?} must not overlap");
    }

    #[test]
    fn an_absent_or_privileged_base_keeps_the_default_window() {
        assert_eq!(window_from(None), RANGE);
        assert_eq!(window_from(Some(80)), RANGE);
        assert_eq!(window_from(Some(1023)), RANGE);
        assert_eq!(window_from(Some(1024)), 1024..1024 + MOVED_WIDTH);
    }

    #[test]
    fn a_base_near_u16_max_clamps_instead_of_wrapping() {
        let w = window_from(Some(u16::MAX - 10));
        assert_eq!(w.end, u16::MAX);
        assert!(w.start < w.end, "window must stay non-empty: {w:?}");
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
