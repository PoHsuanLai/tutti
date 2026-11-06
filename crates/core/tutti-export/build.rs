//! Defines `cfg(codec)`: at least one of the `wav`, `flac`, `aiff` and `ogg`
//! features is on, so the encode path exists.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(codec)");
    let any = ["WAV", "FLAC", "AIFF", "OGG"]
        .iter()
        .any(|f| std::env::var_os(format!("CARGO_FEATURE_{f}")).is_some());
    if any {
        println!("cargo::rustc-cfg=codec");
    }
}
