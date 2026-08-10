//! Construction and input failures.
//!
//! `Result`, not `Option`, and not `assert!`. This is library surface: a
//! caller who passes a hop wider than its window deserves to know which
//! invariant broke, and a caller feeding a short buffer deserves better than a
//! panic on the audio path.

use tutti_types::{Hz, Samples};

/// Everything that can go wrong constructing or feeding an analysis.
#[derive(Debug, Clone, PartialEq)]
pub enum AnalysisError {
    /// A zero-length analysis window.
    ZeroWindow,
    /// A zero hop: frames would never advance.
    ZeroHop,
    /// Frames would not overlap, so the transform cannot reconstruct.
    HopExceedsWindow {
        /// The analysis window, in [`Samples`].
        window: Samples,
        /// The hop, in [`Samples`]; larger than `window`.
        hop: Samples,
    },
    /// Hann² needs `window % hop == 0` and `window / hop >= 4` to
    /// constant-overlap-add. Without it the inverse's normalization is
    /// inexact and untouched bins do not reconstruct.
    NotColaCompliant {
        /// The analysis window, in [`Samples`].
        window: Samples,
        /// The hop, in [`Samples`]; does not divide `window` at 4x overlap.
        hop: Samples,
    },
    /// A sample rate of zero or below.
    NonPositiveSampleRate,
    /// `min >= max`, or either bound at or below zero. Refused at construction
    /// because a detector that accepts it reports "unvoiced" forever instead.
    EmptyFrequencyRange {
        /// The lower bound in [`Hz`]; not below `max`.
        min: Hz,
        /// The upper bound in [`Hz`].
        max: Hz,
    },
    /// A frequency bound above Nyquist for the given rate.
    AboveNyquist {
        /// The offending bound, in [`Hz`].
        freq: Hz,
        /// Half the sample rate, in [`Hz`].
        nyquist: Hz,
    },
    /// A grid's data length disagrees with `frames * bins`.
    GridShapeMismatch {
        /// Values actually present.
        len: usize,
        /// Declared frame count.
        rows: usize,
        /// Declared bin count.
        cols: usize,
    },
    /// Two grids that must share a shape do not.
    GridShapeDisagreement,
    /// Input shorter than the algorithm's minimum.
    InsufficientInput {
        /// The algorithm's minimum, in [`Samples`].
        needed: Samples,
        /// What the caller supplied, in [`Samples`].
        got: Samples,
    },
}

impl core::fmt::Display for AnalysisError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ZeroWindow => write!(f, "analysis window is zero-length"),
            Self::ZeroHop => write!(f, "hop size is zero: frames would not advance"),
            Self::HopExceedsWindow { window, hop } => write!(
                f,
                "hop {hop} exceeds window {window}: frames would not overlap"
            ),
            Self::NotColaCompliant { window, hop } => write!(
                f,
                "window {window} / hop {hop} is not Hann-COLA: needs the hop to \
                 divide the window with at least 4x overlap"
            ),
            Self::NonPositiveSampleRate => write!(f, "sample rate must be positive"),
            Self::EmptyFrequencyRange { min, max } => {
                write!(f, "empty frequency range: min {min} is not below max {max}")
            }
            Self::AboveNyquist { freq, nyquist } => {
                write!(f, "frequency {freq} is above Nyquist {nyquist}")
            }
            Self::GridShapeMismatch { len, rows, cols } => write!(
                f,
                "grid has {len} values but its shape is {rows}x{cols} = {}",
                rows * cols
            ),
            Self::GridShapeDisagreement => {
                write!(f, "grids that must share a shape have different shapes")
            }
            Self::InsufficientInput { needed, got } => {
                write!(f, "need at least {needed} samples, got {got}")
            }
        }
    }
}

impl core::error::Error for AnalysisError {}

/// Result of an analysis call, defaulting the error to [`AnalysisError`].
pub type Result<T> = core::result::Result<T, AnalysisError>;
