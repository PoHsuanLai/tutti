#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![forbid(unsafe_code)]

mod block;
mod driver;
mod driver_seam;
mod error;
mod faults;
mod host;
mod output;
mod owner;

#[cfg(feature = "capture")]
mod mic;

pub use block::OutputBlock;
pub use driver::{DeviceInfo, Stopped, TuttiDriver};
pub use driver_seam::{
    CpalDriver, CpalStream, ManualRunning, ManualStream, ManualStreamDriver, OutputSpec,
    PREFERRED_QUANTUM, RunningStream, StreamDriver,
};
pub use error::{Error, Result};
pub use faults::{StreamFault, StreamFaultKind, StreamFaults};
pub use host::{AudioHost, DeviceHost, DeviceSelector, available_hosts};
pub use output::{AudioCallbackState, AudioEngine, MAX_FRAMES, process_audio};

// Re-exported for the same reason `tutti-midi-file` re-exports `midly`: this
// crate's public surface already names cpal types (`SampleFormat` in
// `OutputSpec`, `StreamError` in `ManualStream::fail`, cpal errors inside
// `Error`), so a consumer needs the vocabulary and should not have to pin a
// matching cpal version of its own.
pub use cpal;

#[cfg(feature = "capture")]
pub use mic::MicIn;
