//! Defines `cfg(codec)`: at least one of the `wav`, `aiff`, `flac` and `vorbis`
//! features is on, so a `Format` has a value and an `Encoder` can be built.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(codec)");
    let any = ["WAV", "AIFF", "FLAC", "VORBIS"]
        .iter()
        .any(|f| std::env::var_os(format!("CARGO_FEATURE_{f}")).is_some());
    if any {
        println!("cargo::rustc-cfg=codec");
    }
}
