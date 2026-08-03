//! macOS compatibility policy.
//!
//! One declared floor, and only as a warning: an older macOS may lack the APFS
//! clonefile (`ws new`'s vendor copy) and subprocess-signal semantics ws relies
//! on. There is deliberately no ceiling — macOS is forward-compatible and any
//! qualified ceiling would trail the latest shipping major, so it would refuse
//! to run for every up-to-date user. A future major that genuinely breaks one
//! of these surfaces at the operation using it, not as a refusal to start.
//!
//! A failed or unparseable `sw_vers` skips the check: Apple has changed that
//! output before, and admitting an unqualified host beats breaking every
//! install on a format bump.

use std::process::Command;

/// The oldest macOS major `ws` is qualified on.
const MIN_MACOS_MAJOR: u32 = 13;

pub fn check() {
    if !cfg!(target_os = "macos") {
        return;
    }
    let Some(running) = current_macos_major() else { return };
    if below_floor(MIN_MACOS_MAJOR, running) {
        eprintln!(
            "warning: ws is qualified for macOS {MIN_MACOS_MAJOR} and newer; running {running} \
             may hit untested APFS/signal behavior."
        );
    }
}

fn below_floor(min: u32, running: u32) -> bool {
    running < min
}

fn current_macos_major() -> Option<u32> {
    let out = Command::new("sw_vers").arg("-productVersion").output().ok()?;
    parse_major(&String::from_utf8_lossy(&out.stdout))
}

/// `None` for anything that doesn't lead with an integer major — the fail-open
/// the module doc promises.
fn parse_major(v: &str) -> Option<u32> {
    v.trim().split('.').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_sw_vers_major() {
        assert_eq!(parse_major("13.4.1"), Some(13));
        assert_eq!(parse_major("26.0\n"), Some(26));
    }

    #[test]
    fn unparseable_sw_vers_fails_open() {
        // Apple has reshaped this output before; a new shape must skip the
        // check, never warn or panic.
        assert_eq!(parse_major(""), None);
        assert_eq!(parse_major("abc"), None);
        assert_eq!(parse_major("Version 13"), None);
    }

    #[test]
    fn only_older_than_the_floor_warns() {
        assert!(below_floor(13, 12), "an older macOS warns");
        assert!(!below_floor(13, 13), "the floor itself is fine");
        // The regression: a build qualified on 14 must still run on macOS 26.
        assert!(!below_floor(13, 26), "a newer macOS must never warn or block");
    }
}
