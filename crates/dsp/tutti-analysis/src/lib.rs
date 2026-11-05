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
//! - Windowed loudness: [`step_windowed_loudness`] with a [`WindowedLoudnessConfig`] and a
//!   [`WindowedLoudnessState`] reads momentary (400 ms), short-term (3 s) and true
//!   peak as a [`WindowedReading`] after every chunk.
//! - Spectrum: [`step_spectrum`] with a [`SpectrumConfig`] and a
//!   [`SpectrumState`] streams power in dB per bin or [`Banding`] band, with
//!   [`Smoothing`], held peaks and a rolling spectrogram.
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
//! - Take alignment: [`align()`] with an [`AlignConfig`] finds an [`Alignment`]
//!   (whole-sample lag, sub-sample offset, confidence) by GCC-PHAT.
//! - Tempo: [`estimate_tempo()`] with a [`TempoConfig`] finds a [`Tempo`] (BPM,
//!   confidence, runner-up octaves); [`track_beats()`] places a [`BeatGrid`] by
//!   dynamic programming; the `_from_novelty` forms take your own curve.
//! - Notes: [`transcribe`] with a [`NoteConfig`] turns a mono buffer into
//!   [`NoteEvent`]s (note, cents bend, span, level); [`notes_from_track`]
//!   takes your own pitch track, [`frame_levels`] and onsets;
//!   [`VelocityRange`] maps a level to a MIDI velocity.
//! - Harmony: [`chroma()`] with a [`ChromaConfig`] folds spectral peaks into
//!   twelve pitch classes and estimates the [`Tuning`], or incrementally with
//!   [`step_chroma`] and a [`ChromaState`]; [`estimate_key`] finds a [`Key`] by
//!   Krumhansl-Schmuckler over a [`KeyProfile`]; [`detect_chords`] with a
//!   [`ChordConfig`] smooths triads into [`ChordSpan`]s by Viterbi.
//! - [`FftScratch`], reusable FFT plans and buffers; [`Grid`] and its typed
//!   indices for frame-by-bin data; [`enum@Error`] and [`Result`].

mod align;
mod beat;
mod chord;
mod chroma;
mod error;
mod features;
mod fft;
mod geometry;
mod grid;
mod key;
mod loudness;
mod notes;
mod onset;
mod peaks;
// No `///` here: a doc comment on a `mod` line shadows the module's own `//!`.
// The module header carries the description.
mod pitch;
mod silence;
mod spectrum;
mod stereo;
mod tempo;
mod transform;
mod vectorscope;
mod window;
mod xcorr;
mod yin;

pub use tutti_types::ChannelLayout;

pub use align::{AlignConfig, Alignment, align};
pub use chord::{ChordConfig, ChordQuality, ChordSpan, detect_chords};
pub use chroma::{Chroma, ChromaConfig, ChromaState, Tuning, chroma, step_chroma};
pub use error::{Error, Result};
pub use features::{
    SpectralFeatures, spectral_centroid, spectral_features, spectral_flatness, spectral_rolloff,
};
pub use fft::FftScratch;
pub use geometry::StftGeometry;
pub use grid::{BinCount, BinIndex, FrameCount, FrameIndex, Grid};
pub use key::{Key, KeyEstimate, KeyProfile, Mode, estimate_key, estimate_key_from_mean};
pub use loudness::{
    Loudness, LoudnessConfig, LoudnessState, WindowedLoudnessConfig, WindowedLoudnessState,
    WindowedReading, finish as finish_loudness, measure_loudness, step_loudness,
    step_windowed_loudness,
};
// `finish` is renamed on the way out, matching `finish_peaks` /
// `finish_loudness` above: three modules each have a `finish`, and the bare name
// says nothing about which accumulator it drains.
pub use notes::{NoteConfig, NoteEvent, VelocityRange, frame_levels, notes_from_track, transcribe};
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
pub use spectrum::{Banding, Smoothing, SpectrumConfig, SpectrumState, step_spectrum};
pub use stereo::{
    Ballistics, BallisticsState, StereoLevels, StereoReading, correlate, step_ballistics,
};
pub use tempo::{
    BeatGrid, Tempo, TempoCandidate, TempoConfig, estimate_tempo, estimate_tempo_from_novelty,
    track_beats, track_beats_from_novelty,
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
