//! Per-OS [`MidiEndpoints`] implementations, and the one place that picks one.
//!
//! # This is the only `cfg(target_os)` that decides anything
//!
//! The bug this rewrite exists to remove was a `#[cfg]` ladder at the *use*
//! site: `MidiOutRouter::route` chose between a macOS-only UMP path and a lossy
//! portable one, so which messages survived depended on where you built. That
//! ladder is unrepresentable once every backend yields the same types, and
//! [`active`] is where the remaining platform choice lives — a **construction**
//! -time `cfg`, not a hot-path one.
//!
//! Adding a backend is therefore additive: one arm here, one module, and nothing
//! downstream changes.
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
//!   [`Error::Unsupported`](crate::core::error::Error::Unsupported).

use crate::core::endpoints::MidiEndpoints;

#[cfg(all(target_os = "macos", feature = "midi-hardware"))]
pub mod coremidi;

pub mod stub;

/// This platform's MIDI backend.
///
/// Returns a boxed trait object rather than an `impl Trait` so the return type
/// does not change per platform — a caller stores it in one field, which is what
/// keeps the `cfg` from leaking outward.
pub fn active() -> Box<dyn MidiEndpoints> {
    #[cfg(all(target_os = "macos", feature = "midi-hardware"))]
    {
        Box::new(coremidi::CoreMidiEndpoints::new())
    }
    #[cfg(not(all(target_os = "macos", feature = "midi-hardware")))]
    {
        Box::new(stub::StubEndpoints::new())
    }
}
