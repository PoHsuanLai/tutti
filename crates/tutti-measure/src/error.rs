//! Failures of the analysers.
//!
//! `Result`, not `Option`, and not `assert!`: a caller who passes a bad
//! setting deserves to know which invariant broke, and a caller feeding a
//! short buffer deserves better than a panic on the audio path.

use tutti_types::{Bpm, Db, Hz, Samples};

/// Everything that can go wrong constructing or feeding an analysis.
///
/// An analyser's own validation raises its own variant. [`Dsp`](Self::Dsp)
/// carries what the shared STFT infrastructure refused (a window and hop that
/// do not fit, a grid whose shape disagrees with its data), so `?` on
/// building a `StftGeometry` or a `Grid` keeps working.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The shared STFT infrastructure refused a geometry or a grid.
    #[error(transparent)]
    Dsp(#[from] tutti_dsp::Error),
    /// A zero hop: frames would never advance.
    #[error("hop size is zero: frames would not advance")]
    ZeroHop,
    /// A sample rate of zero or below.
    #[error("sample rate must be positive")]
    NonPositiveSampleRate,
    /// A threshold in decibels that is NaN or infinite.
    #[error("threshold {threshold} is not finite")]
    InvalidThreshold {
        /// The offending threshold, in [`Db`].
        threshold: Db,
    },
    /// `min >= max`, or either bound at or below zero. Refused at construction
    /// because a detector that accepts it reports "unvoiced" forever instead.
    #[error("empty frequency range: min {min} is not below max {max}")]
    EmptyFrequencyRange {
        /// The lower bound in [`Hz`]; not below `max`.
        min: Hz,
        /// The upper bound in [`Hz`].
        max: Hz,
    },
    /// A frequency bound above Nyquist for the given rate.
    #[error("frequency {freq} is above Nyquist {nyquist}")]
    AboveNyquist {
        /// The offending bound, in [`Hz`].
        freq: Hz,
        /// Half the sample rate, in [`Hz`].
        nyquist: Hz,
    },
    /// A tempo range with `min` not below `max`, or a bound outside what the
    /// analysis searches, or one that leaves no whole-frame lag to search.
    #[error("empty tempo range: min {min} is not below max {max}, or no lag falls in it")]
    EmptyTempoRange {
        /// The slower bound, in [`Bpm`].
        min: Bpm,
        /// The faster bound, in [`Bpm`].
        max: Bpm,
    },
    /// A tempo setting that is not finite or not above zero.
    #[error("invalid tempo parameter: {0}")]
    InvalidTempoParameter(&'static str),
    /// A note-transcription setting or input that is out of range; the text
    /// names it.
    #[error("invalid note parameter: {0}")]
    InvalidNoteParameter(&'static str),
    /// A chroma, key or chord setting or input that is out of range; the text
    /// names it.
    #[error("invalid tonal parameter: {0}")]
    InvalidTonalParameter(&'static str),
    /// A windowed-loudness setting or input that is out of range; the text names it.
    #[error("invalid loudness parameter: {0}")]
    InvalidLoudnessParameter(&'static str),
    /// A spectrum band that holds no bin: the window is too short for it.
    #[error(
        "band {band} ({lower}..{upper}) holds no bin: a window of {window} samples \
         holds every band"
    )]
    EmptyBand {
        /// The first band with no bin, counting from the lowest.
        band: usize,
        /// Its lower edge, in [`Hz`].
        lower: Hz,
        /// Its upper edge, in [`Hz`].
        upper: Hz,
        /// A window, in [`Samples`], at least as wide as the narrowest band
        /// needs, so that every band holds a bin.
        window: Samples,
    },
    /// A spectrum setting or input that is out of range; the text names it.
    #[error("invalid spectrum parameter: {0}")]
    InvalidSpectrumParameter(&'static str),
    /// No pitch to read a key from: no frames, silent, or a chroma so flat
    /// that every key correlates alike.
    #[error("no tonal content: silent or flat chroma")]
    NoTonalContent,
    /// Two grids that must share a shape do not.
    #[error("grids that must share a shape have different shapes")]
    GridShapeDisagreement,
    /// A sample that is NaN or infinite.
    #[error("input holds a NaN or infinite sample")]
    NonFiniteInput,
    /// Nothing to correlate: silent input, or a band holding no spectrum bin.
    #[error("no signal to correlate: silent input or an empty band")]
    NoSignal,
    /// Input shorter than the algorithm's minimum.
    #[error("need at least {needed} samples, got {got}")]
    InsufficientInput {
        /// The algorithm's minimum, in [`Samples`].
        needed: Samples,
        /// What the caller supplied, in [`Samples`].
        got: Samples,
    },
}

/// Result of an analysis call, defaulting the error to [`Error`].
pub type Result<T> = core::result::Result<T, Error>;
