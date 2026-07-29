//! CPAL device layer for Tutti.
//!
//! This is the seam between the engine's DSP graph and a sound card. The graph
//! ([`tutti_core`]) knows how to render a block; this crate knows how to open a
//! device, hand it blocks on time, and pull audio back in from a microphone.
//!
//! ```no_run
//! use tutti_cpal::TuttiDriver;
//!
//! # fn main() -> tutti_cpal::Result<()> {
//! for device in TuttiDriver::devices()? {
//!     println!("{}: {}", device.index, device.name);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! A driver is built from an opened device plus the state its callback reads
//! ([`TuttiDriver::from_parts`]); a host assembles those once at startup.
//!
//! # What belongs here
//!
//! Device enumeration, stream construction, the RT callback body, and mic
//! capture. The pump that drives a mic into a file is `tutti_io::Recorder` —
//! device-free, and one layer down. Everything here is framework-free: a host that wants a different
//! lifecycle (a Bevy `App`, a CLI, a test harness) drives these types itself.
//!
//! The callback ([`process_audio`]) is deliberately a free function taking
//! [`AudioCallbackState`] rather than a method on a driver, so a test can call
//! exactly what CPAL calls without opening a device.
//!
//! # RT discipline
//!
//! [`process_audio`] runs on the audio thread and must not allocate, lock, or
//! block. The buffers it needs are sized once at stream build (to
//! [`MAX_FRAMES`]) and never resized — an over-sized callback is clamped and its
//! tail silenced rather than triggering a reallocation. `tests/rt_no_alloc.rs`
//! gates this against a disabled allocator.

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
