//! Engine bootstrap.
//!
//! The device layer itself lives in [`tutti_cpal`] — stream, callback, mic
//! capture, driver lifecycle — and knows nothing about Bevy. What remains here
//! is the wiring: [`build_into`] runs one ordered fallible RT-wiring
//! transaction and inserts every subsystem as a Bevy resource directly into the
//! `App`, with no intermediate bundle struct to destructure.
//!
//! The device *driver* types are re-exported so a host reaches the whole engine
//! through `bevy_tutti` without naming the device crate. Mic capture and
//! recording are not here — they are the live I/O edge, and live in
//! [`io`](crate::io).

mod build;
pub(crate) mod device_state;
mod error;
mod state;

pub use build::build_into;
pub use device_state::AudioDeviceState;
pub use error::{Error, Result};
pub use state::AudioEngineState;

pub use tutti_cpal::{DeviceInfo, TuttiDriver};

// The mic/record API is not here: it is the live I/O edge, not engine
// bootstrap, so it lives in `crate::io` — one adapter module per engine crate.

// The audio graph is fundsp's `Net` — there is no tutti wrapper. Surfaced here
// so hosts reach it without naming fundsp directly.
pub use tutti_core::dsp::Net;
