use std::process::Command;

const MATRIX_MIN: u32 = 13;

fn main() {
    // Use CARGO_CFG_TARGET_OS — the OS being COMPILED FOR — not `cfg!` which
    // reflects the build host. Matters for cross-compilation (macOS -> Linux
    // and vice versa).
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    let matrix = if target_os == "macos" {
        macos_matrix()
    } else {
        "linux".to_string()
    };
    println!("cargo:rustc-env=WS_COMPAT_MATRIX={matrix}");

    // Rerun triggers:
    //   - build.rs itself changed
    //   - target OS changed (rebuild with a different --target)
    //   - the file `sw_vers` reads for its `-productVersion` output changed
    //     (busts the cache after an OS upgrade)
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CARGO_CFG_TARGET_OS");
    println!("cargo:rerun-if-changed=/System/Library/CoreServices/SystemVersion.plist");
}

fn macos_matrix() -> String {
    // Only invoke `sw_vers` when we're actually building on macOS. When
    // cross-compiling FROM Linux/whatever TO macOS, the host has no `sw_vers`
    // — emit "unknown" and let compat.rs fail-open.
    if !cfg!(target_os = "macos") {
        return "unknown".to_string();
    }

    let Ok(out) = Command::new("sw_vers").arg("-productVersion").output() else {
        return "unknown".to_string();
    };
    let raw = String::from_utf8_lossy(&out.stdout);
    let Some(major) = raw.trim().split('.').next().and_then(|n| n.parse::<u32>().ok()) else {
        return "unknown".to_string();
    };

    // Building on a host older than MATRIX_MIN would produce an inverted
    // range like `13..=12`, which compat::check() would then reject on every
    // supported host. Abort the build with a clear message instead of
    // shipping a poisoned binary.
    assert!(
        major >= MATRIX_MIN,
        "build host macOS {major} is older than MATRIX_MIN ({MATRIX_MIN}). \
         Bump MATRIX_MIN or build on a newer host."
    );

    format!("{MATRIX_MIN}..={major}")
}
