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
//! - Filters: [`SvfCoeffs`] and [`SvfCoefficients`] with [`svf_step`]
//!   ([`SvfType`]); [`LadderCoeffs`] and [`LadderCoefficients`] with
//!   [`ladder_step`] ([`LadderType`]); the Linkwitz-Riley [`Crossover`]; and
//!   [`Real`], the `f32` or `f64` their state runs in.
//! - Dynamics: [`EnvelopeFollower`] and [`GateEnvelopeFollower`]; the gain
//!   curves [`compute_compressor_gain_reduction`], [`compute_gate_gain`],
//!   [`compute_limiter_gain`] and [`LimiterCurve`]; [`CompressorKernel`] and
//!   [`GateKernel`] with their [`CompressorBlock`] and [`GateBlock`]; the
//!   limiter's [`LookaheadRing`]; and the ring buffers [`CircularBuffer`] and
//!   [`MonotonicMinDeque`].
//! - Convolution: [`Convolver`] over a shared [`IrSpectra`].
//! - Delay: [`DelayLine`] with an [`InterpolationMode`], and [`SweptLines`], the
//!   chorus and flanger kernel.
//! - Interpolation: [`cubic_hermite`], the four-tap Catmull-Rom kernel the delay
//!   line and the sampler's readers share.
//! - Reverb: [`Freeverb`], mono in and stereo out, a damped comb bank and
//!   all-pass chain whose comb feedback and damping are set through its
//!   [`Kernel`] impl.
//! - Reverb model (design 022): a [`Network`] of [`Stage`]s, a feedback delay
//!   network as a plain fixed-capacity value, with analytic queries
//!   ([`Network::rt60`], [`tail`](Network::tail), [`is_stable`](Network::is_stable),
//!   the densities) and a [`Bounds`] worst case; a [`Design`] maps its controls
//!   to one ([`FreeverbDesign`] for [`Freeverb`]) and names the [`Kernel`] that runs
//!   it, prepared from a [`Setup`] and moved to each network by `retarget`; [`loop_gain`] and
//!   [`rt60_from_gain`] are the RT60 formula both ways.
//! - Waveshaping: [`ShapeKind`] and [`Shaper`].
//! - Resampling: [`FftResampler`], a fixed-input FFT block resampler on the
//!   same scalar FFT, failing with a [`ResampleError`]; and
//!   [`StreamingResampler`], which feeds it a block of any size at a time and
//!   compensates its delay and tail, sized by a [`ChunkSize`].
//! - Dither: [`DitherNoise`], the seeded rectangular or triangular ([`DitherShape`])
//!   noise that decorrelates quantization, continuous across blocks.
//! - [`Grid`] and its typed
//!   indices for frame-by-bin data; [`enum@Error`] and [`Result`].

mod allpass;
mod analysis;
mod buffer;
mod control;
mod convolver;
mod delay;
mod dither;
mod dynamics;
mod fft;
mod filter;
mod interp;
mod swept_delay;
mod waveshape;

mod network;
mod resample;
mod reverb;
mod reverb_design;
#[cfg(test)]
mod test_support;

pub use allpass::{allpass_chain, allpass_coeff};
pub use analysis::*;
pub use buffer::{CircularBuffer, MonotonicMinDeque};
pub use control::{COEFF_INTERVAL, LANES, Ramp, lane_group, segments};
pub use convolver::{Convolver, IrSpectra};
pub use delay::{DelayLine, InterpolationMode, delay_step, fractional_samples};
pub use dither::{DitherNoise, DitherShape};
pub use dynamics::{
    CompressorBlock, CompressorKernel, EnvelopeFollower, GateBlock, GateEnvelopeFollower,
    GateKernel, LimiterCurve, LookaheadRing, amplitude_to_db, brickwall_clip,
    compute_compressor_gain_reduction, compute_gate_gain, compute_limiter_gain, db_to_amplitude,
    fold_peak, gate_gain_at, ramp_db, ramp_is_at_end, smooth_envelope,
};
pub use fft::{Complex32, FftScratch, RealFft, StagedRealFft};
pub use filter::{
    Crossover, CrossoverError, LadderCoefficients, LadderCoeffs, LadderType, MAX_SPLITS, Real,
    SOLVE_CEILING, SvfCoefficients, SvfCoeffs, SvfType, fit_to_rate, ladder_step, svf_step,
};
pub use interp::cubic_hermite;
pub use network::{
    Absorption, AllPass, AllPassForm, Band, Bounds, Ceiling, Channels, Design, Effort, Fdn,
    FirSummary, Full, Gains, Inline, Kernel, Line, MAX_CHANNELS, MAX_DENSE, MAX_LINES, MAX_STAGES,
    MAX_TAPS, Matrix, Modulation, Network, OutputMap, Ring, Setup, Stage, Tap, Taps, loop_gain,
    pre_delay_frames, rt60_from_gain,
};
pub use resample::{ChunkSize, FftResampler, ResampleError, StreamingResampler};
pub use reverb::Freeverb;
pub use reverb_design::{FreeverbControls, FreeverbDesign};
pub use swept_delay::SweptLines;
pub use tutti_types::ChannelLayout;
pub use waveshape::{ShapeKind, Shaper};

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
/// `tutti-sampler` and the [`Convolver`] work in the same type, so values cross
/// freely between them.
pub use Complex32 as Complex;
