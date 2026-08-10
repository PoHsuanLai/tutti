//! # Tutti Analysis
//!
//! Audio analysis algorithms over `&[f32]`. No framework dependencies, and no
//! opinion about where the results go.
//!
//! - [`stft`] / [`istft_transform`] — the short-time Fourier transform, in
//!   three result types so invertibility is a compile-time question
//! - [`yin()`] — monophonic pitch estimation (de Cheveigné & Kawahara, 2002)
//! - [`detect_onsets`] — onset detection over four selectable detection
//!   functions
//! - [`correlate`] — inter-channel phase correlation and stereo image
//! - [`summarize`] — min/max/RMS waveform blocks for a timeline, **per
//!   channel**; folding to one series is [`PeakBlocks::to_mono`], a caller's
//!   choice rather than this crate's default
//!
//! ## Configs, and carries where they are needed
//!
//! Every algorithm takes a **config**: immutable, validated once, so an
//! invalid combination fails at construction rather than silently producing
//! nothing.
//!
//! Two of them additionally need a **carry** between frames — onset detection
//! diffs against the previous spectrum, and waveform blocking holds a partial
//! block. Those two expose the carry as an explicit value and a `step`
//! function, and their batch entry points ([`detect_onsets`], [`summarize`])
//! *fold that same step*, so the two paths cannot drift. Tests pin the
//! equality across chunk sizes and channel layouts.
//!
//! The rest are stateless: [`yin()`] and [`correlate`] are pure functions of
//! their input, and [`stft`] is batch-only — there is no incremental
//! transform. Meter ballistics ([`step_ballistics`]) carries a smoothed
//! reading, but that is a filter over results rather than a step of the
//! correlation itself.
//!
//! "Live" is a property of a call site, never of an algorithm, so nothing here
//! is named for it. A host that wants these results on a background thread or
//! in an ECS owns that plumbing itself.
//!
//! ## Example
//!
//! ```rust
//! use tutti_analysis::{
//!     correlate, detect_onsets, summarize, yin, DetectionFunction, FftScratch,
//!     OnsetConfig, PeakConfig, StftGeometry, YinConfig,
//! };
//! use tutti_types::{ChannelLayout, Interleaved, Samples, StereoPlanes};
//!
//! let sample_rate = 44100.0;
//! let samples: Vec<f32> = vec![0.0; 44100];
//! let mut fft = FftScratch::new();
//!
//! // Waveform blocks for display — one series per channel.
//! let blocks = summarize(
//!     &PeakConfig::new(Samples(512), ChannelLayout::STEREO),
//!     Interleaved::new(&samples, ChannelLayout::STEREO),
//! );
//! let left = blocks.channel(0).expect("stereo has a channel 0");
//! // A meter wants one number per block; a waveform draws both channels.
//! let merged = blocks.to_mono();
//!
//! // Onsets, via spectral flux.
//! let geometry = StftGeometry::new(sample_rate, Samples(2048), Samples(512))?;
//! let onsets = detect_onsets(
//!     &OnsetConfig::new(geometry, DetectionFunction::SpectralFlux),
//!     &samples,
//!     &mut fft,
//! )?;
//!
//! // Pitch. An inverted range is refused here, not silently unvoiced later.
//! let pitch = yin(&YinConfig::standard(sample_rate)?, &samples)?;
//!
//! // Stereo correlation. The planes are paired once — a length mismatch is
//! // refused here rather than silently truncated inside the measurement.
//! let planes = StereoPlanes::new(&samples, &samples).expect("equal lengths");
//! let reading = correlate(planes);
//! # Ok::<(), tutti_analysis::AnalysisError>(())
//! ```
//!
//! ## Reading from a running graph
//!
//! Nothing here knows about the graph, so the seam is an
//! [`AudioTap`](tutti_core::metering::AudioTap): the audio thread pushes each
//! block into it and a control thread drains it. What arrives on this side is
//! an ordinary `&[f32]`, which is the whole reason these algorithms need no
//! engine vocabulary.
//!
//! ```
//! use tutti_analysis::correlate;
//! use tutti_core::metering::AudioTap;
//! use tutti_types::StereoPlanes;
//!
//! let tap = AudioTap::new();
//! let _consumer = tap.open().expect("a fresh tap has no consumer");
//!
//! // The audio-callback side. `frames` is a FRAME count, so an interleaved
//! // stereo block of 2 frames is 4 samples.
//! let block = [0.5f32, -0.5, 0.5, -0.5];
//! tap.push(&block, 2);
//!
//! // The analysis side, once the drained frames are deinterleaved. Draining
//! // the ring itself needs `ringbuf`'s `Consumer` trait, which is the
//! // consumer's dependency rather than this crate's.
//! let (left, right) = ([0.5f32, 0.5], [-0.5f32, -0.5]);
//! let planes = StereoPlanes::new(&left, &right).expect("drained in lockstep");
//!
//! // `Correlation` is a MEASUREMENT type, deliberately distinct from the
//! // control types (`Mix`, `Depth`) despite the coinciding range.
//! let reading = correlate(planes);
//! ```

pub mod error;
pub mod fft;
pub mod geometry;
pub mod grid;
pub mod loudness;
pub mod onset;
pub mod peaks;
// No `///` here: a doc comment on a `mod` line shadows the module's own `//!`.
// The module header carries the description.
mod pitch;
pub mod stereo;
pub mod transform;
pub mod window;
pub mod yin;

pub use tutti_core::ChannelLayout;

pub use error::{AnalysisError, Result};
pub use fft::FftScratch;
pub use geometry::StftGeometry;
pub use grid::{BinCount, BinIndex, FrameCount, FrameIndex, Grid};
pub use loudness::{
    finish as finish_loudness, measure_loudness, step_loudness, Loudness, LoudnessConfig,
    LoudnessState,
};
pub use onset::{
    complex_domain_deviation, detect_onsets, high_frequency_content, spectral_energy,
    spectral_flux, step_onset, suppress_close_onsets, DetectionFunction, Onset, OnsetConfig,
    OnsetState,
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
pub use window::hann;
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
