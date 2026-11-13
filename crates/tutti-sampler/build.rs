//! Defines `cfg(codec)`: at least one of the `decode-wav`, `decode-flac`, `decode-mp3` and `decode-ogg`
//! features is on, so `tutti-codec` has a decoder to open files with.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(codec)");
    println!("cargo::rerun-if-changed=build.rs");
    let on = |feature: &str| std::env::var_os(format!("CARGO_FEATURE_{feature}")).is_some();
    if ["DECODE_WAV", "DECODE_FLAC", "DECODE_MP3", "DECODE_OGG"]
        .into_iter()
        .any(on)
    {
        println!("cargo::rustc-cfg=codec");
    }
}
