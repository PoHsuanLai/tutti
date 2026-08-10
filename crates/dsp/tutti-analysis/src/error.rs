//! Construction and input failures.
//!
//! `Result`, not `Option`, and not `assert!`. This is library surface: a
//! caller who passes a hop wider than its window deserves to know which
//! invariant broke, and a caller feeding a short buffer deserves better than a
//! panic on the audio path.

use tutti_types::{Hz, Samples};

use crate::window::{CosineWindow, Window};

/// Everything that can go wrong constructing or feeding an analysis.
#[derive(Debug, Clone, PartialEq)]
pub enum AnalysisError {
    /// A zero-length analysis window.
    ZeroWindow,
    /// A zero hop: frames would never advance.
    ZeroHop,
    /// Frames would not overlap, so the transform cannot reconstruct.
    HopExceedsWindow { window: Samples, hop: Samples },
    /// A window squared needs `window % hop == 0` and an overlap of at least
    /// its own [`cola_overlap`](crate::CosineWindow::cola_overlap) to
    /// constant-overlap-add. Without it the inverse's normalization is inexact
    /// and untouched bins do not reconstruct.
    ///
    /// Carries the window shape because the required overlap depends on it —
    /// 4x for Hann and Hamming, 8x for Blackman — so an error naming only the
    /// pair could not say what it needed.
    NotColaCompliant {
        window: Samples,
        hop: Samples,
        window_fn: CosineWindow,
    },
    /// A sample rate of zero or below.
    NonPositiveSampleRate,
    /// `min >= max`, or either bound at or below zero. The old detector
    /// accepted this and then reported "unvoiced" forever.
    EmptyFrequencyRange { min: Hz, max: Hz },
    /// A frequency bound above Nyquist for the given rate.
    AboveNyquist { freq: Hz, nyquist: Hz },
    /// A grid's data length disagrees with `frames * bins`.
    GridShapeMismatch {
        len: usize,
        rows: usize,
        cols: usize,
    },
    /// Two grids that must share a shape do not.
    GridShapeDisagreement,
    /// Input shorter than the algorithm's minimum.
    InsufficientInput { needed: Samples, got: Samples },
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
            Self::NotColaCompliant {
                window,
                hop,
                window_fn,
            } => write!(
                f,
                "window {window} / hop {hop} is not {window_fn:?}-COLA: needs the hop to \
                 divide the window with at least {}x overlap",
                window_fn.cola_overlap()
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

pub type Result<T> = core::result::Result<T, AnalysisError>;
