//! Turn the `tutti-alsa-ump-sys` probe result into this crate's cfgs.
//!
//! - `alsa_ump`: alsa-lib >= 1.2.10, the UMP sequencer API the Linux backend
//!   needs.
//! - `alsa_ump_create`: alsa-lib >= 1.2.13, which adds publishing our own UMP
//!   endpoint.
//!
//! An older or absent alsa-lib degrades to the empty stub rather than failing
//! the build; the sys crate's build script prints the reason as a warning.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Declared unconditionally so `unexpected_cfgs` stays quiet where unset.
    println!("cargo:rustc-check-cfg=cfg(alsa_ump)");
    println!("cargo:rustc-check-cfg=cfg(alsa_ump_create)");

    // Cargo exports the sys crate's `cargo:KEY=` lines as `DEP_ASOUND_KEY`,
    // only on Linux, where it is a dependency.
    if std::env::var_os("DEP_ASOUND_ALSA_UMP").is_some() {
        println!("cargo:rustc-cfg=alsa_ump");
    }
    if std::env::var_os("DEP_ASOUND_ALSA_UMP_CREATE").is_some() {
        println!("cargo:rustc-cfg=alsa_ump_create");
    }
}
