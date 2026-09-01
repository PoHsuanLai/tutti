//! Audio analysis algorithms over `&[f32]`. No framework dependencies, and no
//! opinion about where the results go.
//!
//! - [`stft`] / [`istft_transform`] — the short-time Fourier transform, in
//!   three result types so invertibility is a compile-time question
//! - [`yin()`] — monophonic pitch estimation (de Cheveigné & Kawahara, 2002)
//! - [`detect_onsets`] — onset detection over four selectable detection
//!   functions
//! - [`correlate`] — inter-channel phase correlation and stereo image
//! - [`measure_loudness`] — integrated loudness
//! - [`summarize`] — min/max/RMS waveform blocks for a timeline, **per
//!   channel**; folding to one series is [`PeakBlocks::to_mono`], a caller's
//!   choice rather than this crate's default
//!
//! Every algorithm takes an immutable, validated [`Error`]-returning config, so
//! an invalid combination fails at construction rather than silently producing
//! nothing. Two of them ([`detect_onsets`], [`summarize`]) additionally carry
//! state between frames and expose it as an explicit value plus a `step`
//! function, which their batch entry points fold — so the incremental and batch
//! paths cannot drift.
//!
//! The quick start, the graph-tap seam, the fallibility rule and the features
//! are in the crate README, included below.
#![doc = include_str!("../README.md")]

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
/// need first. It lives in `tutti-types` beside the ITU-R BS.775 matrices
/// rather than here, so app-side consumers can reach it without depending on
/// this crate — five of them had hand-rolled their own, two silently dropping
/// channels 2..N.
pub use tutti_types::{fold_buffer_to_mono, fold_planar_to_mono};

/// The generic complex type, re-exported so consumers can name a bin without
/// depending on `rustfft` directly. Most code wants [`Complex`] instead.
pub use rustfft::num_complex::Complex as GenericComplex;

/// A single frequency bin: a rectangular complex number.
///
/// A plain alias rather than a newtype, deliberately. Wrapping it would buy
/// backend-swappability this crate does not want, and cost either `unsafe`
/// transmutes at the `rustfft` boundary or a conversion on every bin of the
/// spectral edit path's mask multiply. `num_complex::Complex<f32>` is stable,
/// ubiquitous, and structurally transparent — there is no ambiguity for a
/// newtype to close, unlike the unit types, where a bare `f32` genuinely does
/// not say whether it means Hz or seconds.
///
/// **Why `rustfft` here** when the rest of the engine uses vendored fundsp's
/// FFT (`tutti_core::Complex32`): analysis windows are arbitrary-size and
/// cold-path, so `rustfft`'s planner and SIMD are the right trade. microfft is
/// fixed-size and allocation-free, which is what the realtime graph needs and
/// this crate does not. Both are `num_complex::Complex<f32>` underneath, so
/// values cross freely — only the transform differs.
pub type Complex = rustfft::num_complex::Complex<f32>;
