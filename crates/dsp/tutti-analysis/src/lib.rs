#![doc = include_str!("../README.md")]
//!
//! ## Items
//!
//! - Transform: [`stft`], [`stft_magnitude`], [`stft_polar`] and
//!   [`istft_transform`], over a validated [`StftGeometry`] with a
//!   [`CosineWindow`], producing [`Stft`], [`StftMagnitude`] or [`StftPolar`].
//! - Pitch: [`yin()`] and [`yin_track`] with a [`YinConfig`], returning
//!   [`PitchEstimate`] and [`Pitch`]; [`median_filter`] and [`penalize_jumps`]
//!   clean up a track.
//! - Onsets: [`detect_onsets`] with an [`OnsetConfig`] and a
//!   [`DetectionFunction`], or incrementally with [`step_onset`] and
//!   [`finish_onset`].
//! - Waveform blocks: [`summarize`] with a [`PeakConfig`] into [`PeakBlocks`],
//!   or incrementally with [`step_peaks`] and [`finish_peaks`].
//! - Loudness: [`measure_loudness`] with a [`LoudnessConfig`] into
//!   [`Loudness`], or incrementally with [`step_loudness`] and
//!   [`finish_loudness`].
//! - Stereo: [`correlate`] into a [`StereoReading`], smoothed for a meter by
//!   [`step_ballistics`].
//! - [`FftScratch`], reusable FFT plans and buffers; [`Grid`] and its typed
//!   indices for frame-by-bin data; [`enum@Error`] and [`Result`].

mod error;
mod fft;
mod geometry;
mod grid;
mod loudness;
mod onset;
mod peaks;
// No `///` here: a doc comment on a `mod` line shadows the module's own `//!`.
// The module header carries the description.
mod pitch;
mod stereo;
mod transform;
mod window;
mod yin;

pub use tutti_core::ChannelLayout;

pub use error::{Error, Result};
pub use fft::FftScratch;
pub use geometry::StftGeometry;
pub use grid::{BinCount, BinIndex, FrameCount, FrameIndex, Grid};
pub use loudness::{
    finish as finish_loudness, measure_loudness, step_loudness, Loudness, LoudnessConfig,
    LoudnessState,
};
// `finish` is renamed on the way out, matching `finish_peaks` /
// `finish_loudness` above: three modules each have a `finish`, and the bare name
// says nothing about which accumulator it drains.
pub use onset::{
    complex_domain_deviation, detect_onsets, finish as finish_onset, high_frequency_content,
    spectral_energy, spectral_flux, step_onset, suppress_close_onsets, DetectionFunction, Onset,
    OnsetConfig, OnsetState,
};
pub use peaks::{
    finish as finish_peaks, step_peaks, summarize, summarize_block, PeakAccum, PeakBlock,
    PeakBlocks, PeakConfig, PeakState,
};
pub use stereo::{
    correlate, step_ballistics, Ballistics, BallisticsState, StereoLevels, StereoReading,
};
pub use transform::{
    istft as istft_transform, stft, stft_magnitude, stft_polar, HopPolicy, NormalizedMagnitudes,
    RawMagnitudes, SampleRange, Stft, StftMagnitude, StftPolar, StftRequest,
};
pub use window::{CosineWindow, Window};
pub use yin::{median_filter, penalize_jumps, yin, yin_track, Pitch, PitchEstimate, YinConfig};

/// Notes and pitch classes, re-exported from the engine vocabulary: a
/// [`Pitch`] names one, and callers should not need a second import to read it.
pub use tutti_types::{Note, PitchClass};

/// Buffer-level mono folding, re-exported from the engine's downmix module.
///
/// Every STFT and pitch entry point takes mono, so this is the step callers
/// usually need first. Folding keeps every channel, rather than dropping
/// channels past the second.
pub use tutti_types::{fold_buffer_to_mono, fold_planar_to_mono};

/// The generic complex type, re-exported so consumers can name a bin without
/// depending on `rustfft` directly. Most code wants [`Complex`] instead.
pub use rustfft::num_complex::Complex as GenericComplex;

/// A single frequency bin: a rectangular complex number.
///
/// A plain alias for `num_complex::Complex<f32>`, the type `rustfft` works
/// in, so bins cross the FFT boundary without conversion. The engine's
/// real-time FFT (`microfft`, in `tutti-sampler`) uses the same type, so values
/// cross freely between the two.
pub type Complex = rustfft::num_complex::Complex<f32>;
