//! Time-stretching and pitch-shifting via phase vocoder.
//!
//! Changes duration WITHOUT changing pitch, or pitch without duration — the
//! operation varispeed cannot express, since resampling couples the two. See
//! [`PlaybackRate`](tutti_core::PlaybackRate) for the coupled kind.
//!
//! [`Unit`] is a pure frame-in → frame-out filter: it owns no source, so the
//! caller ticks its own source and feeds each frame in. That is why this is a
//! peer of `playback` rather than part of it — nothing here knows what a clip
//! is.
//!
//! # Example
//!
//! ```ignore
//! use tutti_sampler::{SamplerUnit, stretch};
//! use std::sync::Arc;
//!
//! // Create a sampler with a loaded audio file
//! let mut sampler = SamplerUnit::new(Arc::new(wave));
//!
//! // The stretcher is a pure filter: the caller ticks `sampler` and feeds each
//! // frame into `stretched` (it owns no source of its own).
//! let mut stretched = stretch::Unit::new(44100.0);
//!
//! // Slow down to half speed
//! stretched.set_stretch_factor(2.0);
//!
//! // Pitch up by one octave
//! stretched.set_pitch_cents(1200.0);
//! ```
//!
//! # Features
//!
//! - **Lock-free parameter updates**: Real-time control via atomic operations
//! - **High-quality phase vocoder**: Phase-locked algorithm for pitched content
//! - **Multiple FFT sizes**: Trade-off between latency and quality
//! - **Stereo processing**: Independent left/right channel processing
//!

mod phase_vocoder;
mod types;
mod unit;

pub use types::{Algorithm, FftSize, Params};
pub use unit::Unit;
