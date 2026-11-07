//! Defines `cfg(encode)` and `cfg(decode)`: at least one `encode-*` feature is
//! on, so a `Format` has a value and an `Encoder` can be built; at least one
//! `decode-*` feature is on, so a file can be decoded.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(encode)");
    println!("cargo::rustc-check-cfg=cfg(decode)");
    let any = |features: &[&str]| {
        features
            .iter()
            .any(|f| std::env::var_os(format!("CARGO_FEATURE_{f}")).is_some())
    };
    if any(&["ENCODE_WAV", "ENCODE_AIFF", "ENCODE_FLAC", "ENCODE_VORBIS"]) {
        println!("cargo::rustc-cfg=encode");
    }
    if any(&["DECODE_WAV", "DECODE_FLAC", "DECODE_MP3", "DECODE_OGG"]) {
        println!("cargo::rustc-cfg=decode");
    }
}
