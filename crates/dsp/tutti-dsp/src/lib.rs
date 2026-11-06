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
//! - FFT: [`RealFft`], a real-input FFT on rustfft's scalar planner whose
//!   arithmetic does not depend on the CPU; [`StagedRealFft`], the same
//!   transform in steps; [`FftScratch`], reusable plans and buffers.
//! - [`Grid`] and its typed
//!   indices for frame-by-bin data; [`enum@Error`] and [`Result`].

mod analysis;
mod fft;

pub use analysis::*;
pub use fft::{Complex32, FftScratch, RealFft, StagedRealFft};
pub use tutti_types::ChannelLayout;

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
/// `num_complex::Complex<f32>`, the type [`RealFft`] works in, so
/// bins cross the FFT boundary without conversion. The vocoder in
/// `tutti-sampler` and the convolver in `tutti-nodes` work in the same type, so
/// values cross freely between them.
pub use Complex32 as Complex;
