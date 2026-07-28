//! macOS compatibility policy.
//!
//! `ws` depends on macOS-specific semantics that have drifted across recent majors:
//!   - APFS clonefile (used by `ws new`'s vendor/ copy via `cp -c`).
//!   - Subprocess signal defaults (hook orchestration).
//!   - Gatekeeper quarantine handling — matters for `ws update` payloads (v2),
//!     pre-qualifying now.
//!
//! Each release ships with a MIN..=MAX matrix of qualified macOS majors, baked in
//! at build time via `build.rs`. Only the **floor** gates behavior, and only as a
//! warning: an *older*-than-qualified macOS may lack the APFS/signal semantics
//! above, so we say so — but never block. A *newer* major is ignored entirely:
//! macOS is forward-compatible, the build host's major always trails the latest
//! shipping macOS, and a hard ceiling would refuse to run for every up-to-date
//! user. If a future major genuinely breaks one of these semantics, it surfaces
//! at the operation that uses it, not as a blanket refusal to start.
//!
//! If `sw_vers` fails or the matrix is unparseable, we skip the check — Apple has
//! changed `sw_vers` output before, and admitting an unqualified host beats
//! breaking every install on a format bump.

use std::process::Command;

const MATRIX: &str = env!("WS_COMPAT_MATRIX");

pub fn check() {
    if MATRIX == "linux" || MATRIX == "unknown" {
        return;
    }

    let Some((min, _max)) = parse_range(MATRIX) else { return; };
    let Some(running) = current_macos_major() else { return; };

    if below_floor(min, running) {
        eprintln!(
            "warning: ws is qualified for macOS {min} and newer; running {running} \
             may hit untested APFS/signal behavior."
        );
    }
}

/// Warn only below the qualified floor. A newer-than-qualified major is
/// forward-compatible and must never warn or block — the build host trails the
/// latest shipping macOS, so a ceiling check would fire for every current user.
fn below_floor(min: u32, running: u32) -> bool {
    running < min
}

fn parse_range(s: &str) -> Option<(u32, u32)> {
    let (lo, hi) = s.split_once("..=")?;
    Some((lo.parse().ok()?, hi.parse().ok()?))
}

fn current_macos_major() -> Option<u32> {
    let out = Command::new("sw_vers").arg("-productVersion").output().ok()?;
    let v = String::from_utf8_lossy(&out.stdout);
    v.trim().split('.').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_matrix_range() {
        assert_eq!(parse_range("13..=15"), Some((13, 15)));
        assert_eq!(parse_range("10..=10"), Some((10, 10)));
    }

    #[test]
    fn rejects_malformed_range() {
        assert_eq!(parse_range("linux"), None);
        assert_eq!(parse_range("13..15"), None);
        assert_eq!(parse_range("13..=abc"), None);
        assert_eq!(parse_range(""), None);
    }

    #[test]
    fn only_older_than_the_floor_warns() {
        assert!(below_floor(13, 12), "an older macOS warns");
        assert!(!below_floor(13, 13), "the floor itself is fine");
        // The regression: a build qualified on 14 must still run on macOS 26.
        assert!(!below_floor(13, 26), "a newer macOS must never warn or block");
    }
}
