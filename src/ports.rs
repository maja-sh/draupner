//! Per-service host-port allocation persisted in
//! `.ws-meta/<len>_<branch>-<len>_<repo>.json`, stable across `up` runs and
//! collision-free across parallel worktrees. The length-prefix on each
//! segment closes a `-`-split ambiguity where `(feat-web, api)` and
//! `(feat, web-api)` used to share the same filename.

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

/// Filename stem = `<len>_<branch>-<len>_<repo>`. Length-prefixed segments
/// so `(feat-web, api)` and `(feat, web-api)` can't collapse to the same
/// file via the `-` split — bare concatenation had those two tuples both
/// producing `feat-web-api.json`, so a `.ws-meta` file could be shared
/// between two live tasks and their port maps would step on each other.
fn meta_stem(branch_sanitized: &str, repo: &str) -> String {
    format!("{}_{branch_sanitized}-{}_{repo}", branch_sanitized.len(), repo.len())
}

fn meta_file(root: &Path, branch_sanitized: &str, repo: &str) -> PathBuf {
    meta_dir(root).join(format!("{}.json", meta_stem(branch_sanitized, repo)))
}

pub fn override_file(root: &Path, branch_sanitized: &str, repo: &str) -> PathBuf {
    meta_dir(root).join(override_filename(branch_sanitized, repo))
}

/// Just the basename — the remote path builder assembles its own dir prefix
/// but still needs the identity-collision-proof filename shape.
pub fn override_filename(branch_sanitized: &str, repo: &str) -> String {
    format!("{}.override.yml", meta_stem(branch_sanitized, repo))
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Meta {
    #[serde(default)]
    ports: BTreeMap<String, u16>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    services: BTreeMap<String, ServiceConfig>,
}

/// A missing file is fine (nothing recorded yet); every other IO or parse
/// error propagates so allocation paths (`resolve`, `used_ports`,
/// `record_services`) refuse to write around a corrupt file — otherwise a
/// truncated meta silently reallocates while old containers still hold
/// those ports.
fn read_meta(path: &Path) -> Result<Meta> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Meta::default()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
}

/// Best-effort read for the status/render paths (`load`, `services`) that
/// must not short-circuit a whole listing on one corrupt file. A note
/// surfaces the corruption so it isn't invisible.
fn read_meta_lossy(path: &Path) -> Meta {
    match read_meta(path) {
        Ok(m) => m,
        Err(e) => {
            crate::ui::note(&format!("ignoring corrupt {}: {e:#}", path.display()));
            Meta::default()
        }
    }
}

fn write_meta(path: &Path, meta: &Meta) -> Result<()> {
    crate::config::write_json_pretty(path, meta)
}

pub fn load(root: &Path, branch_sanitized: &str, repo: &str) -> BTreeMap<String, u16> {
    read_meta_lossy(&meta_file(root, branch_sanitized, repo)).ports
}

/// Effective service set from the last `up` so derived-but-undeclared services
/// still resolve their host/port on read paths.
pub fn services(root: &Path, branch_sanitized: &str, repo: &str) -> BTreeMap<String, ServiceConfig> {
    read_meta_lossy(&meta_file(root, branch_sanitized, repo)).services
}

/// Both `ports` and `services` in one read — for callers that need both and would otherwise parse the same `.ws-meta` file twice.
pub fn load_meta(
    root: &Path,
    branch_sanitized: &str,
    repo: &str,
) -> (BTreeMap<String, u16>, BTreeMap<String, ServiceConfig>) {
    let m = read_meta_lossy(&meta_file(root, branch_sanitized, repo));
    (m.ports, m.services)
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
    let mut meta = read_meta(&path)?;
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
    let existing = read_meta(&path)?;
    if needs.iter().all(|s| existing.ports.contains_key(s)) {
        return Ok(existing.ports);
    }

    let _lock = Lock::acquire(&dir);
    // Re-read under the lock; another process may have allocated since our fast-path read.
    let mut meta = read_meta(&path)?;
    let mut used = used_ports(&dir)?;
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

/// Best-effort drop; a missing file is fine at teardown. Held under the same
/// lock as `resolve` so a concurrent allocation's atomic rename can't land
/// after our removal and leave a fresh-but-forgotten meta behind.
pub fn forget(root: &Path, branch_sanitized: &str, repo: &str) {
    let _lock = Lock::acquire(&meta_dir(root));
    let _ = std::fs::remove_file(meta_file(root, branch_sanitized, repo));
    let _ = std::fs::remove_file(override_file(root, branch_sanitized, repo));
}

/// Every port recorded across `.ws-meta`, so parallel worktrees never collide.
///
/// A missing `.ws-meta` dir is fine — no allocations yet. A corrupt peer
/// file must NOT be treated as empty: that peer's ports would drop off the
/// used set and the allocator would hand them out again while the peer's
/// containers are still bound to them.
fn used_ports(dir: &Path) -> Result<HashSet<u16>> {
    let mut set = HashSet::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(set),
        Err(e) => return Err(e).with_context(|| format!("scan {}", dir.display())),
    };
    for e in rd.filter_map(std::result::Result::ok) {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) == Some("json") {
            set.extend(read_meta(&p)?.ports.values().copied());
        }
    }
    Ok(set)
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
    /// Cookie written into the lock file at acquire time. `None` means acquire
    /// fell through (e.g. read-only dir) and drop must stay a no-op.
    cookie: Option<String>,
}

const LOCK_WINDOW: Duration = Duration::from_secs(5);

/// `pid-nanos` — unique per acquire because two live processes can't share a
/// PID and one process can't call `acquire` twice in the same nanosecond.
/// Readable in the file so a stuck lock is diagnosable with `cat .lock`.
fn lock_cookie() -> String {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{pid}-{nanos}")
}

impl Lock {
    fn acquire(dir: &Path) -> Lock {
        let path = dir.join(".lock");
        let cookie = lock_cookie();
        let mut deadline = Instant::now() + LOCK_WINDOW;
        loop {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut f) => {
                    use std::io::Write;
                    // A best-effort write: even if it fails, drop's cookie
                    // check just reads an empty file and doesn't match, so the
                    // worst case is that we don't clean up our own lock —
                    // never that we clobber someone else's.
                    let _ = f.write_all(cookie.as_bytes());
                    let _ = f.sync_all();
                    return Lock { path, cookie: Some(cookie) };
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if Instant::now() >= deadline {
                        // Steal, then reset the deadline so a legitimate re-acquirer isn't thrashed.
                        let _ = std::fs::remove_file(&path);
                        deadline = Instant::now() + LOCK_WINDOW;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                // Other errors (e.g. read-only dir): give up on locking but still allocate.
                Err(_) => return Lock { path, cookie: None },
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let Some(cookie) = &self.cookie else { return };
        // The cookie check exists exactly for the case where our deadline
        // expired, someone else stole and re-acquired, and unlinking now
        // would leave them unlocked without knowing.
        let Ok(actual) = std::fs::read_to_string(&self.path) else { return };
        if actual.trim() == cookie {
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

    #[test]
    fn read_meta_defaults_on_missing_but_surfaces_a_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.json");
        assert!(
            read_meta(&missing).unwrap().ports.is_empty(),
            "a missing file must default — nothing recorded yet",
        );

        let corrupt = dir.path().join("bad.json");
        std::fs::write(&corrupt, "{not json").unwrap();
        // `unwrap_err` needs `Meta: Debug`, which serves no callers otherwise.
        let err = match read_meta(&corrupt) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected a parse error on corrupt input"),
        };
        assert!(err.contains("parse"), "should surface as a parse error: {err}");
    }

    #[test]
    fn used_ports_refuses_to_ignore_a_corrupt_peer() {
        // A corrupt peer file, if silently defaulted, drops the peer's ports off the used
        // set and the allocator hands them out again — exactly the bug this refusal fixes.
        let dir = tempfile::tempdir().unwrap();
        let meta = meta_dir(dir.path());
        std::fs::create_dir_all(&meta).unwrap();
        std::fs::write(meta.join("feat-a-api.json"), r#"{"ports":{"mysql":20005}}"#).unwrap();
        std::fs::write(meta.join("feat-b-api.json"), "garbage").unwrap();
        assert!(used_ports(&meta).is_err(), "a corrupt file must propagate, not silently drop ports");
    }

    #[test]
    fn resolve_refuses_to_reallocate_over_a_corrupt_meta() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let meta = meta_dir(root);
        std::fs::create_dir_all(&meta).unwrap();
        std::fs::write(meta_file(root, "feat-x", "api"), "not json").unwrap();
        let err = resolve(&Site::Local, root, "feat-x", "api", &["mysql".into()]).unwrap_err();
        assert!(err.to_string().contains("parse"), "expected a parse-error propagation: {err}");
    }

    #[test]
    fn load_stays_lossy_on_a_corrupt_file() {
        // A single corrupt task must not break `wsx list` for its siblings; the strict
        // refusal belongs to the allocation path only.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let meta = meta_dir(root);
        std::fs::create_dir_all(&meta).unwrap();
        std::fs::write(meta_file(root, "feat-x", "api"), "corrupt").unwrap();
        assert!(load(root, "feat-x", "api").is_empty(), "lossy path defaults on a corrupt file");
    }

    #[test]
    fn lock_drop_removes_when_the_cookie_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".lock");
        let lock = Lock::acquire(dir.path());
        assert!(path.exists(), "acquire must create the lock file");
        drop(lock);
        assert!(!path.exists(), "matching cookie must remove on drop");
    }

    #[test]
    fn lock_drop_leaves_a_foreign_cookie_alone() {
        // The scenario: A's steal window expires, B removes A's file and
        // creates B's fresh one; then A's drop finally fires. Without the
        // cookie check, A would unlink B's live lock and leave B unlocked.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".lock");
        let a = Lock::acquire(dir.path());
        std::fs::remove_file(&path).expect("simulate the steal");
        std::fs::write(&path, "999-someone-elses-cookie").expect("write B's cookie");
        drop(a);
        assert!(path.exists(), "A's drop must not unlink B's fresh lock");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "999-someone-elses-cookie",
            "B's cookie must survive A's drop",
        );
    }

    #[test]
    fn meta_file_does_not_collide_across_dash_split() {
        // Bare `<branch>-<repo>.json` collapsed these two tuples onto the same
        // filename, so two live tasks shared one port map and clobbered each
        // other's allocations. Length-prefixed segments are provably unique.
        let root = std::path::Path::new("/ws");
        let a = meta_file(root, "feat-web", "api");
        let b = meta_file(root, "feat", "web-api");
        assert_ne!(a, b, "distinct tuples must produce distinct meta files: {a:?} vs {b:?}");

        // Same guarantee for the override yml.
        let a = override_file(root, "feat-web", "api");
        let b = override_file(root, "feat", "web-api");
        assert_ne!(a, b, "distinct tuples must produce distinct overrides: {a:?} vs {b:?}");
    }

    #[test]
    fn lock_drop_is_a_noop_when_acquire_fell_through() {
        // Acquire against a non-existent directory yields `Lock { cookie: None }`;
        // drop must stay a no-op instead of panicking or removing something.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        drop(Lock::acquire(&missing));
    }
}
