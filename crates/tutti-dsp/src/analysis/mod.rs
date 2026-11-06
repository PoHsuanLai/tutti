//! Analyzers over plain sample slices.

mod align;
mod beat;
mod chord;
mod chroma;
mod error;
mod features;
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

pub use align::{AlignConfig, Alignment, align};
pub use chord::{ChordConfig, ChordQuality, ChordSpan, detect_chords};
pub use chroma::{Chroma, ChromaConfig, ChromaState, Tuning, chroma, step_chroma};
pub use error::{Error, Result};
pub use features::{
    SpectralFeatures, spectral_centroid, spectral_features, spectral_flatness, spectral_rolloff,
};
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
