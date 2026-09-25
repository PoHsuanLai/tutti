#![doc = include_str!("../README.md")]

mod block;
mod driver;
mod driver_seam;
mod error;
mod faults;
mod host;
mod output;

#[cfg(feature = "capture")]
mod mic;

pub use block::OutputBlock;
pub use driver::{DeviceInfo, Stopped, TuttiDriver};
pub use driver_seam::{
    CpalDriver, CpalStream, ManualRunning, ManualStream, ManualStreamDriver, OutputSpec,
    RunningStream, StreamDriver,
};
pub use error::{Error, Result};
pub use faults::{StreamFault, StreamFaultKind, StreamFaults};
pub use host::{available_hosts, AudioHost, DeviceHost, DeviceSelector};
pub use output::{process_audio, AudioCallbackState, AudioEngine, MAX_FRAMES};

// Re-exported for the same reason `tutti-midi-file` re-exports `midly`: this
// crate's public surface already names cpal types (`SampleFormat` in
// `OutputSpec`, `StreamError` in `ManualStream::fail`, cpal errors inside
// `Error`), so a consumer needs the vocabulary and should not have to pin a
// matching cpal version of its own.
pub use cpal;

#[cfg(feature = "capture")]
pub use mic::MicIn;
