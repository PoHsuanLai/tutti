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
//! let summary = compute_summary(&samples, 1, 512);
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
pub mod istft;
pub mod live;
pub mod pitch;
pub mod spectrum;
pub mod stft;
pub mod transient;
pub mod waveform;

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

/// Re-exported so consumers of [`ComplexStftResult`] / `istft_complex` can name
/// the complex bin type without depending on `rustfft` directly.
pub use rustfft::num_complex::Complex;
pub use transient::{DetectionMethod, Transient, TransientDetector};
pub use waveform::{MultiResolutionSummary, WaveformBlock, WaveformSummary};
