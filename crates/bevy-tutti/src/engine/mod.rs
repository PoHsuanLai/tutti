//! Engine bootstrap.
//!
//! The device layer itself lives in [`tutti_cpal`] — stream, callback, mic
//! capture, driver lifecycle — and knows nothing about Bevy. What remains here
//! is the wiring: [`build_into`] runs one ordered fallible RT-wiring
//! transaction and inserts every subsystem as a Bevy resource directly into the
//! `App`, with no intermediate bundle struct to destructure.
//!
//! The device types are re-exported so a host reaches the whole engine through
//! `bevy_tutti` without naming the device crate.

mod build;
mod error;

pub use build::build_into;
pub use error::{Error, Result};

pub use tutti_cpal::{DeviceInfo, TuttiDriver};

// Mic capture and the live mic→WAV recorder. Gated on `sampler` because both
// speak `tutti_sampler`'s `AudioIn`/`AudioOut` traits, so the sampler is only a
// dependency under that feature.
#[cfg(feature = "sampler")]
pub use tutti_cpal::{MicIn, Recorder};
// The live-monitor graph node paired with `MicIn::open_with_monitor`. Defined
// in the (device-free) sampler; surfaced here so the whole mic API — capture,
// record, monitor — is reachable from one place.
#[cfg(feature = "sampler")]
pub use tutti_sampler::MicMonitorNode;

// The audio graph is fundsp's `Net` — there is no tutti wrapper. Surfaced here
// so hosts reach it without naming fundsp directly.
pub use tutti_core::dsp::Net;
