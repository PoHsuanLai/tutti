//! The Tutti audio engine, owned by bevy-tutti.
//!
//! Relocated from the former standalone `tutti` umbrella crate when bevy-tutti
//! became the umbrella. Holds the CPAL audio callback (`audio_io`), the device
//! lifecycle (`driver`), the editable DSP graph (`graph`), and the engine
//! bootstrap (`build`). [`build_into`] runs one ordered fallible RT-wiring
//! transaction and inserts every subsystem as a Bevy resource directly into the
//! `App` — there is no intermediate bundle struct to destructure.
//!
//! Unlike the old umbrella these are NOT gated on a `std` feature — bevy-tutti
//! always runs with std (it drives a Bevy `App` over CPAL).

mod audio_io;
mod build;
mod driver;
mod error;

// Microphone capture as an `AudioIn` — the input-device twin of `audio_io`'s
// output stream. Gated on `sampler` because it implements `tutti_sampler`'s
// `AudioIn` trait (and recording pumps a `MicIn` into a sampler `WavOut`).
#[cfg(feature = "sampler")]
mod mic;

// The live mic→WAV driver: pumps a `MicIn` into a sampler `WavOut` on a
// background thread. Gated on `sampler` to match `mic` — it drives that source.
#[cfg(feature = "sampler")]
mod recorder;

#[cfg(all(feature = "midi", feature = "export"))]
pub mod midi_export;

pub use build::{build_into, DefaultProcessor};
pub use driver::{DeviceInfo, TuttiDriver};
pub use error::{Error, Result};
#[cfg(feature = "sampler")]
pub use mic::MicIn;
// The live-monitor graph node paired with `MicIn::open_with_monitor`.
// Defined in the (device-free) sampler; re-exported here so the whole mic API —
// capture, record, monitor — is reachable from one place. Gated on `sampler`
// like its `MicIn`/`Recorder` neighbours: `tutti-sampler` is only a dep under
// that feature, so an ungated re-export breaks the no-sampler build.
#[cfg(feature = "sampler")]
pub use tutti_sampler::MicMonitorNode;
#[cfg(feature = "sampler")]
pub use recorder::Recorder;
// `AudioGraph` (plus `isolate_output` / `GraphDot`) live in tutti-core's `graph`
// module; the engine surfaces them so existing `engine::AudioGraph` paths hold.
pub use tutti_core::{isolate_output, AudioGraph, GraphDot};
