//! # Tutti Analysis
//!
//! Audio analysis algorithms over `&[f32]`. No framework dependencies, and no
//! opinion about where the results go.
//!
//! - [`stft`] / [`istft_transform`] — the short-time Fourier transform, in
//!   three result types so invertibility is a compile-time question
//! - [`yin`] — monophonic pitch estimation (de Cheveigné & Kawahara, 2002)
//! - [`detect_onsets`] — onset detection over four selectable detection
//!   functions
//! - [`correlate`] — inter-channel phase correlation and stereo image
//! - [`summarize`] — min/max/RMS waveform blocks for a timeline
//!
//! ## Batch and streaming are the same code
//!
//! Each algorithm is a **config** (immutable, validated once), an explicit
//! **carry** where one is genuinely needed, and a **step** function. Batch
//! entry points fold the step, so the two paths cannot drift:
//!
//! ```text
//! fold(step, cfg, state, frames) == frames.map(|f| step(cfg, &mut state, f))
//! ```
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
//! use tutti_types::{ChannelLayout, Samples};
//!
//! let sample_rate = 44100.0;
//! let samples: Vec<f32> = vec![0.0; 44100];
//! let mut fft = FftScratch::new();
//!
//! // Waveform blocks for display.
//! let blocks = summarize(
//!     &PeakConfig::new(Samples(512), ChannelLayout::Mono),
//!     &samples,
//! );
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
//! // Stereo correlation.
//! let reading = correlate(&samples, &samples);
//! # Ok::<(), tutti_analysis::AnalysisError>(())
//! ```

pub mod cache;
pub mod correlation;
pub mod error;
pub mod fft;
pub mod geometry;
pub mod grid;
pub mod istft;
pub mod onset;
pub mod peaks;
pub mod pitch;
pub mod stereo;
pub mod stft;
pub mod transform;
pub mod transient;
pub mod waveform;
pub mod window;
pub mod yin;

pub use tutti_core::ChannelLayout;

pub use error::{AnalysisError, Result};
pub use fft::FftScratch;
pub use geometry::StftGeometry;
pub use grid::{BinCount, BinIndex, FrameCount, FrameIndex, Grid};
pub use onset::{
    complex_domain_deviation, detect_onsets, high_frequency_content, spectral_energy,
    spectral_flux, step_onset, suppress_close_onsets, DetectionFunction, Onset, OnsetConfig,
    OnsetState,
};
pub use peaks::{
    finish as finish_peaks, step_peaks, summarize, summarize_block, PeakBlock, PeakConfig,
    PeakState,
};
pub use stereo::{
    correlate, step_ballistics, Ballistics, BallisticsState, StereoLevels, StereoReading,
};
pub use transform::{
    istft as istft_transform, stft, stft_magnitude, stft_polar, HopPolicy, NormalizedMagnitudes,
    RawMagnitudes, SampleRange, Stft, StftMagnitude, StftPolar, StftRequest,
};
pub use window::hann;
pub use yin::{
    frequency_to_note, note_to_frequency, yin, yin_track, Pitch, PitchEstimate, YinConfig,
};

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
pub use pitch::{
    freq_to_midi, median_filter, midi_to_freq, viterbi_smooth, PitchDetector, PitchResult,
};
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
