//! Decide whether this build can have a Linux UMP backend, and say so if not.
//!
//! ALSA's UMP sequencer API has **two** version floors, and they differ:
//!
//! - **1.2.10** — `snd_seq_ump_event_*`, `snd_seq_set_client_midi_version`,
//!   `snd_seq_set_client_ump_conversion`, the `snd_ump_block_info_*` getters.
//!   Everything the backend actually needs. Sets `cfg(alsa_ump)`.
//! - **1.2.13** — `snd_seq_create_ump_endpoint` / `snd_seq_create_ump_block`,
//!   for *publishing* our own UMP endpoint rather than connecting to someone
//!   else's. Sets `cfg(alsa_ump_create)`.
//!
//! # It degrades, it does not fail
//!
//! Ubuntu 22.04 ships alsa-lib 1.2.6.1, and CI installs whatever
//! `libasound2-dev` the runner has. A `panic!` here would break the build for
//! every such machine over a feature they may not use, so an old (or absent)
//! alsa-lib emits a `cargo:warning` and compiles the same empty stub Windows
//! gets: zero endpoints, and `Error::Unsupported` naming the reason.
//!
//! Deliberately **not** `dlopen` + weak symbols. That would turn a build-time
//! diagnosis into a runtime NULL deref on the MIDI thread.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    // Declare the cfgs unconditionally so `unexpected_cfgs` stays quiet on the
    // platforms that never set them.
    println!("cargo:rustc-check-cfg=cfg(alsa_ump)");
    println!("cargo:rustc-check-cfg=cfg(alsa_ump_create)");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }
    let Some(version) = alsa_version() else {
        println!(
            "cargo:warning=alsa-lib not found via pkg-config; building without the Linux \
             MIDI backend. Install libasound2-dev >= 1.2.10 for native UMP."
        );
        return;
    };

    let Some((major, minor, patch)) = parse_version(&version) else {
        println!("cargo:warning=could not parse alsa-lib version {version:?}; assuming too old");
        return;
    };

    if (major, minor, patch) < (1, 2, 10) {
        println!(
            "cargo:warning=alsa-lib {version} is older than 1.2.10, which is where the UMP \
             sequencer API landed; building without the Linux MIDI backend. MIDI files and \
             the software MIDI bus are unaffected."
        );
        return;
    }

    println!("cargo:rustc-link-lib=asound");
    if let Some(dir) = pkg_config_var("libdir") {
        println!("cargo:rustc-link-search=native={dir}");
    }
    println!("cargo:rustc-cfg=alsa_ump");

    if (major, minor, patch) >= (1, 2, 13) {
        println!("cargo:rustc-cfg=alsa_ump_create");
    } else {
        println!(
            "cargo:warning=alsa-lib {version} has the UMP sequencer API but not \
             snd_seq_create_ump_endpoint (1.2.13+); tutti can use MIDI endpoints but not \
             publish its own."
        );
    }
}

fn alsa_version() -> Option<String> {
    pkg_config_output(&["--modversion", "alsa"])
}

fn pkg_config_var(var: &str) -> Option<String> {
    pkg_config_output(&[&format!("--variable={var}"), "alsa"])
}

fn pkg_config_output(args: &[&str]) -> Option<String> {
    let out = Command::new("pkg-config").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// `"1.2.14"` → `(1, 2, 14)`. A missing patch reads as 0, which is right:
/// "1.2" means 1.2.0, and that is below every floor here.
fn parse_version(v: &str) -> Option<(u32, u32, u32)> {
    let mut parts = v.split('.').map(|p| {
        p.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse::<u32>()
            .unwrap_or(0)
    });
    Some((
        parts.next()?,
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    ))
}
