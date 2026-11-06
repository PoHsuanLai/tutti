//! Construction and input failures.
//!
//! `Result`, not `Option`, and not `assert!`. This is library surface: a
//! caller who passes a hop wider than its window deserves to know which
//! invariant broke, and a caller feeding a short buffer deserves better than a
//! panic on the audio path.

use tutti_types::{Bpm, Db, Hz, Samples};

use crate::analysis::window::{CosineWindow, Window};

/// Everything that can go wrong constructing or feeding an analysis.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A zero-length analysis window.
    #[error("analysis window is zero-length")]
    ZeroWindow,
    /// A zero hop: frames would never advance.
    #[error("hop size is zero: frames would not advance")]
    ZeroHop,
    /// Frames would not overlap, so the transform cannot reconstruct.
    #[error("hop {hop} exceeds window {window}: frames would not overlap")]
    HopExceedsWindow {
        /// The analysis window, in [`Samples`].
        window: Samples,
        /// The hop, in [`Samples`]; larger than `window`.
        hop: Samples,
    },
    /// A window squared needs `window % hop == 0` and an overlap of at least
    /// its own [`cola_overlap`](crate::CosineWindow::cola_overlap) to
    /// constant-overlap-add. Without it the inverse's normalization is inexact
    /// and untouched bins do not reconstruct.
    ///
    /// Carries the window shape because the required overlap depends on it —
    /// 4x for Hann and Hamming, 8x for Blackman — so an error naming only the
    /// pair could not say what it needed.
    #[error(
        "window {window} / hop {hop} is not {window_fn:?}-COLA: needs the hop to \
         divide the window with at least {}x overlap",
        window_fn.cola_overlap()
    )]
    NotColaCompliant {
        /// The analysis window, in [`Samples`].
        window: Samples,
        /// The hop, in [`Samples`]; does not divide `window` at the overlap
        /// `window_fn` requires.
        hop: Samples,
        /// The window shape, which is what sets the required overlap.
        window_fn: CosineWindow,
    },
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
    /// A tempo range with `min` not below `max`, or a bound that is not finite
    /// and above zero, or one that leaves no whole-frame lag to search.
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
    /// A grid's data length disagrees with `frames * bins`.
    #[error("grid has {len} values but its shape is {rows}x{cols} = {}", rows * cols)]
    GridShapeMismatch {
        /// Values actually present.
        len: usize,
        /// Declared frame count.
        rows: usize,
        /// Declared bin count.
        cols: usize,
    },
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
