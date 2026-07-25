//! # Tutti Analysis
//!
//! Audio analysis tools for DAW applications.
//!
//! This crate provides efficient algorithms for:
//! - **Waveform thumbnails**: Multi-resolution min/max/RMS summaries for visualization
//! - **Transient detection**: Onset/beat detection using spectral flux and other methods
//! - **Pitch detection**: Monophonic pitch tracking using the YIN algorithm
//! - **Stereo correlation**: Phase correlation, stereo width, and balance analysis
//! - **Live analysis**: Real-time analysis state with lock-free updates
//! - **Thumbnail cache**: LRU cache for waveform thumbnails
//!
//! All functions operate on raw `&[f32]` sample buffers - no framework dependencies.
//!
//! ## Example
//!
//! ```rust
//! use tutti_analysis::{
//!     ChannelLayout,
//!     waveform::compute_summary,
//!     transient::TransientDetector,
//!     pitch::PitchDetector,
//!     correlation::CorrelationMeter,
//! };
//!
//! let samples: Vec<f32> = vec![0.0; 44100]; // 1 second of audio
//! let sample_rate = 44100.0;
//!
//! // Waveform thumbnail
//! let summary = compute_summary(&samples, ChannelLayout::Mono, 512);
//!
//! // Transient detection
//! let mut detector = TransientDetector::new(sample_rate);
//! let transients = detector.detect(&samples);
//!
//! // Pitch detection (needs at least buffer_size() samples)
//! let mut pitch_detector = PitchDetector::new(sample_rate);
//! let pitch = pitch_detector.detect(&samples);
//!
//! // Stereo correlation (for stereo audio)
//! let left = &samples[..];
//! let right = &samples[..];
//! let mut meter = CorrelationMeter::new(sample_rate);
//! let analysis = meter.process(left, right);
//! ```

pub mod cache;
pub mod correlation;
pub mod error;
pub mod fft;
pub mod geometry;
pub mod grid;
pub mod istft;
pub mod live;
pub mod pitch;
pub mod spectrum;
pub mod stft;
pub mod transform;
pub mod transient;
pub mod waveform;
pub mod window;

pub use tutti_core::ChannelLayout;

pub use error::{AnalysisError, Result};
pub use fft::FftScratch;
pub use geometry::StftGeometry;
pub use grid::{BinCount, BinIndex, FrameCount, FrameIndex, Grid};
pub use transform::{
    istft as istft_transform, stft, stft_magnitude, stft_polar, HopPolicy, NormalizedMagnitudes,
    RawMagnitudes, SampleRange, Stft, StftMagnitude, StftPolar, StftRequest,
};
pub use window::hann;

/// Buffer-level mono folding, re-exported from the engine's downmix module.
///
/// Every STFT and pitch entry point takes mono, so this is the step callers
/// need first. It lives in `tutti-types` beside the ITU-R BS.775 matrices
/// rather than here, so app-side consumers can reach it without depending on
/// this crate — five of them had hand-rolled their own, two silently dropping
/// channels 2..N.
pub use tutti_types::{fold_buffer_to_mono, fold_planar_to_mono};

pub use cache::ThumbnailCache;
pub use correlation::{CorrelationMeter, StereoAnalysis};
pub use istft::{istft, istft_complex};
pub use live::{run_analysis_thread, LiveAnalysisState};
// Bevy ECS surface of the live-analysis duty — co-located in `live` with the
// RT engine it mirrors. Gated behind the `bevy` feature.
#[cfg(feature = "bevy")]
pub use live::{
    AnalysisRes, DisableLiveAnalysis, EnableLiveAnalysis, LiveAnalysisData, PendingAnalysis,
    TuttiAnalysisPlugin,
};
pub use pitch::{
    freq_to_midi, median_filter, midi_to_freq, viterbi_smooth, PitchDetector, PitchResult,
};
pub use spectrum::SpectrumResult;
pub use stft::{
    compute_stft, compute_stft_complex, compute_stft_range, hann_cola_ok, ComplexStftResult,
    IncrementalStftBuilder, StftResult,
};

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
pub use transient::{DetectionMethod, Transient, TransientDetector};
pub use waveform::{MultiResolutionSummary, WaveformBlock, WaveformSummary};
