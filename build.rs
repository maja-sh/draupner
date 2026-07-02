use std::process::Command;

const MATRIX_MIN: u32 = 13;

fn main() {
    let matrix = if cfg!(target_os = "macos") {
        let major = Command::new("sw_vers")
            .arg("-productVersion")
            .output()
            .ok()
            .and_then(|out| {
                let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
                v.split('.').next().and_then(|n| n.parse::<u32>().ok())
            });
        match major {
            Some(m) => format!("{MATRIX_MIN}..={m}"),
            None => "unknown".to_string(),
        }
    } else {
        "linux".to_string()
    };
    println!("cargo:rustc-env=WS_COMPAT_MATRIX={matrix}");

    println!("cargo:rerun-if-changed=build.rs");
}
