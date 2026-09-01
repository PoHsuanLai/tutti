#![doc = include_str!("../README.md")]

mod driver;
mod error;
mod output;

#[cfg(feature = "capture")]
mod mic;

pub use driver::{DeviceInfo, TuttiDriver};
pub use error::{Error, Result};
pub use output::{process_audio, AudioCallbackState, AudioEngine, MAX_FRAMES};

#[cfg(feature = "capture")]
pub use mic::MicIn;
