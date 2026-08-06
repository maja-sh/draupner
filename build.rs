use std::process::Command;

const MATRIX_MIN: u32 = 13;

fn main() {
    // CARGO_CFG_TARGET_OS is the compile target; cfg! here would reflect the build host and break cross-compilation.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    let matrix = if target_os == "macos" {
        macos_matrix()
    } else {
        "linux".to_string()
    };
    println!("cargo:rustc-env=WS_COMPAT_MATRIX={matrix}");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CARGO_CFG_TARGET_OS");
    // Watch the plist sw_vers reads so the cache busts after a macOS upgrade.
    println!("cargo:rerun-if-changed=/System/Library/CoreServices/SystemVersion.plist");
}

fn macos_matrix() -> String {
    // Cross-compiling to macOS from another host has no `sw_vers`; leave detection to compat.rs.
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

    // A host older than MATRIX_MIN would emit an inverted range like `13..=12` that compat::check() rejects everywhere.
    assert!(
        major >= MATRIX_MIN,
        "build host macOS {major} is older than MATRIX_MIN ({MATRIX_MIN}). \
         Bump MATRIX_MIN or build on a newer host."
    );

    format!("{MATRIX_MIN}..={major}")
}
