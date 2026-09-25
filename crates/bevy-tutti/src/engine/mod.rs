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
//! recording are not here — they are the live I/O edge, and live in `crate::io`
//! (compiled with the `audio-io` feature).

pub(crate) mod build;
pub(crate) mod device_state;
mod restart;
mod state;

pub use crate::error::{Error, Result};
pub use build::build_into;
pub use device_state::AudioDeviceState;
pub use restart::{restart_device, restart_device_on, DeviceRestart};
pub use state::AudioEngineState;

pub use tutti_cpal::{DeviceInfo, TuttiDriver};

// The mic/record API is not here: it is the live I/O edge, not engine
// bootstrap, so it lives in `crate::io` — one adapter module per engine crate.

// The live graph is `AudioGraphRes`, which keeps its `Net` private. `Net` is
// still surfaced for the offline side: an export's `prepare` hook is handed the
// net it renders (`PreparedNet`), and a host reaches that without naming fundsp.
pub use tutti_core::dsp::Net;
