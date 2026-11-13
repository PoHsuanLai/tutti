//! Turn the `tutti-alsa-ffi` probe result into this crate's cfgs.
//!
//! - `alsa_ump_create`: alsa-lib >= 1.2.13, which adds publishing our own UMP
//!   endpoint.
//! - `midi_backend`: the one platform decision. `"coremidi"` on macOS, `"alsa"`
//!   on Linux with alsa-lib >= 1.2.10 (the UMP sequencer API that backend
//!   needs), `"none"` everywhere else. The `backend` module declares the
//!   platform module this names; no other file branches on the platform.
//!
//! An older or absent alsa-lib degrades to the empty stub rather than failing
//! the build; the ffi crate's build script prints the reason as a warning.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Declared unconditionally so `unexpected_cfgs` stays quiet where unset.
    println!("cargo:rustc-check-cfg=cfg(alsa_ump_create)");
    println!(r#"cargo:rustc-check-cfg=cfg(midi_backend, values("coremidi", "alsa", "none"))"#);

    // Cargo exports the ffi crate's `cargo:KEY=` lines as `DEP_ASOUND_KEY`,
    // only on Linux, where it is a dependency.
    if std::env::var_os("DEP_ASOUND_ALSA_UMP_CREATE").is_some() {
        println!("cargo:rustc-cfg=alsa_ump_create");
    }

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let backend = if target_os == "macos" {
        "coremidi"
    } else if target_os == "linux" && std::env::var_os("DEP_ASOUND_ALSA_UMP").is_some() {
        "alsa"
    } else {
        "none"
    };
    println!("cargo:rustc-cfg=midi_backend=\"{backend}\"");
}
