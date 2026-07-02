//! macOS compatibility policy.
//!
//! `ws` depends on macOS-specific semantics that have drifted across recent majors:
//!   - APFS clonefile (used by `ws new`'s vendor/ copy via `cp -c`).
//!   - Gatekeeper behavior (relevant to `ws update` payloads).
//!   - Subprocess signal defaults (hook orchestration).
//!
//! Each release ships with a MIN..=MAX matrix of qualified macOS majors,
//! baked in at build time via `build.rs`. Outside that range we refuse rather
//! than silently break. If `sw_vers` fails or the matrix is unparseable, we
//! skip the check — Apple has changed `sw_vers` output before, and it's better
//! to admit an unqualified host than to break every install on a format bump.

use std::process::Command;

const MATRIX: &str = env!("WS_COMPAT_MATRIX");

pub fn check() {
    if MATRIX == "linux" || MATRIX == "unknown" {
        return;
    }

    let Some((min, max)) = parse_range(MATRIX) else { return; };
    let Some(running) = current_macos_major() else { return; };

    if running < min || running > max {
        eprintln!(
            "macOS compatibility matrix mismatch: build qualified against \
             macOS {MATRIX}, running {running}. To ship a build for macOS \
             {running}, someone with source access needs to run the \
             qualification on that major first."
        );
        std::process::exit(2);
    }
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
}
