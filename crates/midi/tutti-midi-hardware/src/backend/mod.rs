//! Per-OS [`MidiEndpoints`] implementations, and the one place that picks one.
//!
//! # This is the only `cfg(target_os)` that decides anything
//!
//! Every backend yields the same types, so which messages survive does not
//! depend on where you built. [`active`] is where the platform choice lives: a
//! **construction**-time `cfg`, not a hot-path one. Adding a backend is one arm
//! here and one module; nothing downstream changes.
//!
//! # The platforms, and why they differ
//!
//! - **macOS** — CoreMIDI, native UMP. `MIDIInputPortCreateWithProtocol` /
//!   `MIDISendEventList` carry UMP words end to end.
//! - **Linux** — ALSA seq-UMP (`snd_seq_ump_event_*`), needing alsa-lib ≥ 1.2.10.
//!   Gated on the `alsa_ump` cfg that `build.rs` sets from `pkg-config`, so a
//!   distro with an older alsa-lib still *builds* — it reports no endpoints and
//!   says why, rather than failing to compile.
//! - **Everything else** — [`stub`]. Windows has no usable UMP API in any
//!   `windows` crate version (WinRT `Devices.Midi` is MIDI-1.0 message types;
//!   Windows MIDI Services is not bound), so it enumerates nothing and returns
//!   [`Error::Unsupported`](crate::error::Error::Unsupported).

use crate::endpoints::MidiEndpoints;

#[cfg(target_os = "macos")]
pub mod coremidi;

// No `///` on a `pub mod`: it shadows the module's own `//!` and re-resolves
// that module's intra-doc links in this scope. `alsa/mod.rs` carries the text.
//
// Present only when `build.rs` found alsa-lib >= 1.2.10. Below that floor the
// `alsa_ump` cfg is unset and `stub` takes over, with a `cargo:warning`
// explaining why — see `build.rs`.
#[cfg(all(target_os = "linux", alsa_ump))]
pub mod alsa;

pub mod stub;

/// Returns this platform's MIDI backend (the stub where there is none).
///
/// Returns a boxed trait object rather than an `impl Trait` so the return type
/// does not change per platform — a caller stores it in one field, which is what
/// keeps the `cfg` from leaking outward.
pub fn active() -> Box<dyn MidiEndpoints> {
    #[cfg(target_os = "macos")]
    {
        Box::new(coremidi::CoreMidiEndpoints::new())
    }
    #[cfg(all(target_os = "linux", alsa_ump))]
    {
        Box::new(alsa::AlsaEndpoints::new())
    }
    // Windows, and a Linux whose alsa-lib predates the UMP API.
    #[cfg(not(any(target_os = "macos", all(target_os = "linux", alsa_ump))))]
    {
        Box::new(stub::StubEndpoints::new())
    }
}
