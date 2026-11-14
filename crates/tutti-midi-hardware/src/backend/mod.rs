//! Per-OS [`MidiEndpoints`](crate::MidiEndpoints) implementations, and the one place that picks one.
//!
//! # The platform choice is the `platform` module
//!
//! Every backend yields the same types, so which messages survive does not
//! depend on where you built. `build.rs` names the backend in the
//! `midi_backend` cfg, and the `cfg` below chooses one `platform` module from
//! it: each provides the same surface (`active`, `open_virtual_source` and
//! `open_virtual_destination`). Nothing else in the crate branches on the platform. Adding a
//! backend is one `platform` file and one line here.
//!
//! # The platforms, and why they differ
//!
//! - **macOS** — CoreMIDI, native UMP. `MIDIInputPortCreateWithProtocol` /
//!   `MIDISendEventList` carry UMP words end to end.
//! - **Linux** — ALSA seq-UMP (`snd_seq_ump_event_*`), needing alsa-lib ≥ 1.2.10.
//!   Chosen only when `build.rs` finds that version through `pkg-config`, so a
//!   distro with an older alsa-lib still *builds* — it reports no endpoints and
//!   says why, rather than failing to compile.
//! - **Windows** — Windows Multimedia (winmm), MIDI 1.0 bytes. The OS has no
//!   UMP in this API, so a port is listed as MIDI 1.0 and its bytes are carried
//!   as MIDI 1.0 UMP, as a legacy ALSA port is: `legacy_port` holds that logic
//!   (it runs on every platform, and its tests with it) and `winmm` the calls.
//!   Windows MIDI Services, the UMP-native API, is not bound.
//! - **Everything else** — [`stub`]: it enumerates nothing and returns
//!   [`Error::Unsupported`](crate::error::Error::Unsupported).

#[cfg(midi_backend = "coremidi")]
mod coremidi;

// Present only when `build.rs` found alsa-lib >= 1.2.10. Below that floor the
// backend is `none`, with a `cargo:warning` explaining why: see `build.rs`.
#[cfg(midi_backend = "alsa")]
mod alsa;

#[cfg(midi_backend = "winmm")]
mod winmm;

// What a backend over a MIDI 1.0 byte API decides, with no OS call in it:
// always compiled so its tests run everywhere; only `winmm` uses it as a
// backend.
#[cfg_attr(
    not(midi_backend = "winmm"),
    allow(
        dead_code,
        reason = "the byte-port backend is not this platform's, so only the tests use it"
    )
)]
mod legacy_port;

// Always compiled so its tests run everywhere; only the `none` platform uses
// it as the backend.
#[cfg_attr(
    not(midi_backend = "none"),
    allow(
        dead_code,
        reason = "a native backend is active, so only the tests use the stub"
    )
)]
mod stub;

#[cfg(midi_backend = "coremidi")]
#[path = "platform/coremidi.rs"]
mod platform;

#[cfg(midi_backend = "alsa")]
#[path = "platform/alsa.rs"]
mod platform;

#[cfg(midi_backend = "winmm")]
#[path = "platform/winmm.rs"]
mod platform;

#[cfg(midi_backend = "none")]
#[path = "platform/none.rs"]
mod platform;

// The platform's surface: `active` and the two virtual-endpoint constructors.
// Each returns a boxed trait object rather than an `impl Trait` so the return
// type does not change per platform: a caller stores it in one field.
pub(crate) use platform::*;
