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
//! ([`TuttiDriver::from_parts`]); a host assembles those once at startup:
//!
//! ```no_run
//! use std::sync::Arc;
//! use tutti_core::dsp::{sine_hz, Net};
//! use tutti_core::{AudioTap, Engine, MasterMeter, Transport, TransportClock};
//! use tutti_cpal::{AudioCallbackState, AudioEngine, TuttiDriver};
//!
//! # fn main() -> tutti_cpal::Result<()> {
//! // Open a device first: it reports the rate the graph must be built at.
//! let mut audio_engine = AudioEngine::new(None)?;
//! let sample_rate = audio_engine.sample_rate();
//!
//! let transport = Transport::new(sample_rate);
//! let mut net = Net::new(0, 2);
//! net.push(Box::new(TransportClock::new(
//!     transport.clock_links(),
//!     sample_rate,
//! )));
//! let tone = net.push(Box::new(sine_hz::<f32>(440.0)));
//! net.pipe_output(tone);
//!
//! // `backend()` is the audio thread's half of the graph; the control thread
//! // keeps `net` and `commit`s edits across to it.
//! let engine = Engine::new(transport.motion.clone(), net.backend());
//! let state = Arc::new(AudioCallbackState::new(
//!     engine,
//!     MasterMeter::new(),
//!     AudioTap::new(),
//! ));
//!
//! audio_engine.start(Arc::clone(&state))?;
//! let driver = TuttiDriver::from_parts(audio_engine, state);
//! assert!(driver.is_running());
//! # Ok(())
//! # }
//! ```
//!
//! `no_run` rather than runnable: every line type-checks, but `AudioEngine::new`
//! opens a real sound card, which a test runner has no business doing.
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
