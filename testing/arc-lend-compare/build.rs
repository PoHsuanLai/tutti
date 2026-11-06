//! Records the compiler version, so the harness can print the toolchain that
//! built the code it measures (not whichever `rustc` is on `PATH` at run time).

use std::process::Command;

fn main() {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let version = Command::new(rustc)
        .arg("-V")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=ARC_LEND_COMPARE_RUSTC={version}");
    println!("cargo:rerun-if-env-changed=RUSTC");
}
