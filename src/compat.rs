//! Warn if the running macOS major is below the qualified floor; never block, since
//! macOS is forward-compatible and the build host always trails the latest shipping
//! major. Parse failures skip the check rather than break every install.

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
            "warning: wsx is qualified for macOS {min} and newer; running {running} \
             may hit untested APFS/signal behavior."
        );
    }
}

/// A newer-than-qualified major never warns; only the floor gates.
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
