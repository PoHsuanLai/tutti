//! Defines `cfg(codec)`: at least one of the `wav`, `flac`, `mp3` and `ogg`
//! features is on, so `tutti-io` has a decoder to open files with.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(codec)");
    println!("cargo::rerun-if-changed=build.rs");
    let on = |feature: &str| std::env::var_os(format!("CARGO_FEATURE_{feature}")).is_some();
    if ["WAV", "FLAC", "MP3", "OGG"].into_iter().any(on) {
        println!("cargo::rustc-cfg=codec");
    }
}
