#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]
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
//!   [`finish_onset`]; [`OnsetState::novelty`] reads the raw detection curve.
//! - Waveform blocks: [`summarize`] with a [`PeakConfig`] into [`PeakBlocks`],
//!   or incrementally with [`step_peaks`] and [`finish_peaks`].
//! - Loudness: [`measure_loudness`] with a [`LoudnessConfig`] into
//!   [`Loudness`], or incrementally with [`step_loudness`] and
//!   [`finish_loudness`].
//! - Stereo: [`correlate`] into a [`StereoReading`], smoothed for a meter by
//!   [`step_ballistics`].
//! - Spectral shape: [`spectral_centroid`], [`spectral_flatness`] and
//!   [`spectral_rolloff`] over one magnitude spectrum, or [`spectral_features`]
//!   over every frame of a [`StftMagnitude`], into [`SpectralFeatures`].
//! - Strip silence: [`strip_silence`] with a [`SilenceConfig`] into
//!   [`Region`]s, or incrementally with [`step_silence`] and
//!   [`finish_silence`], which reads the config the scan used.
//! - Vectorscope: [`vectorscope_points`] turns a stereo pair into
//!   [`ScopePoint`]s (side, mid), or block by block with [`step_vectorscope`]
//!   and a [`VectorscopeState`].
//! - Cross-correlation: [`cross_correlate`] into a [`CrossCorrelation`], read
//!   at a lag or for its [`CorrelationPeak`].
//! - [`FftScratch`], reusable FFT plans and buffers; [`Grid`] and its typed
//!   indices for frame-by-bin data; [`enum@Error`] and [`Result`].

mod error;
mod features;
mod fft;
mod geometry;
mod grid;
mod loudness;
mod onset;
mod peaks;
// No `///` here: a doc comment on a `mod` line shadows the module's own `//!`.
// The module header carries the description.
mod pitch;
mod silence;
mod stereo;
mod transform;
mod vectorscope;
mod window;
mod xcorr;
mod yin;

pub use tutti_types::ChannelLayout;

pub use error::{Error, Result};
pub use features::{
    SpectralFeatures, spectral_centroid, spectral_features, spectral_flatness, spectral_rolloff,
};
pub use fft::FftScratch;
pub use geometry::StftGeometry;
pub use grid::{BinCount, BinIndex, FrameCount, FrameIndex, Grid};
pub use loudness::{
    Loudness, LoudnessConfig, LoudnessState, finish as finish_loudness, measure_loudness,
    step_loudness,
};
// `finish` is renamed on the way out, matching `finish_peaks` /
// `finish_loudness` above: three modules each have a `finish`, and the bare name
// says nothing about which accumulator it drains.
pub use onset::{
    DetectionFunction, Onset, OnsetConfig, OnsetState, complex_domain_deviation, detect_onsets,
    finish as finish_onset, high_frequency_content, spectral_energy, spectral_flux, step_onset,
    suppress_close_onsets,
};
pub use peaks::{
    PeakAccum, PeakBlock, PeakBlocks, PeakConfig, PeakState, finish as finish_peaks, step_peaks,
    summarize, summarize_block,
};
pub use silence::{
    Region, SilenceConfig, SilenceState, finish as finish_silence, step_silence, strip_silence,
};
pub use stereo::{
    Ballistics, BallisticsState, StereoLevels, StereoReading, correlate, step_ballistics,
};
pub use transform::{
    HopPolicy, NormalizedMagnitudes, RawMagnitudes, SampleRange, Stft, StftMagnitude, StftPolar,
    StftRequest, istft as istft_transform, stft, stft_magnitude, stft_polar,
};
pub use vectorscope::{ScopePoint, VectorscopeState, step_vectorscope, vectorscope_points};
pub use window::{CosineWindow, Window};
pub use xcorr::{CorrelationPeak, CrossCorrelation, cross_correlate};
pub use yin::{Pitch, PitchEstimate, YinConfig, median_filter, penalize_jumps, yin, yin_track};

/// Notes and pitch classes, re-exported from the engine vocabulary: a
/// [`Pitch`] names one, and callers should not need a second import to read it.
pub use tutti_types::{Note, PitchClass};

/// Buffer-level mono folding, re-exported from the engine's downmix module.
///
/// Every STFT and pitch entry point takes mono, so this is the step callers
/// usually need first. Folding keeps every channel, rather than dropping
/// channels past the second.
pub use tutti_types::{fold_buffer_to_mono, fold_planar_to_mono};

/// A single frequency bin: a rectangular complex number.
///
/// `num_complex::Complex<f32>`, the type [`tutti_types::RealFft`] works in, so
/// bins cross the FFT boundary without conversion. The vocoder in
/// `tutti-sampler` and the convolver in `tutti-nodes` work in the same type, so
/// values cross freely between them.
pub use tutti_types::Complex32 as Complex;
