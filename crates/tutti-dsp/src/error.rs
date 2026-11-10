//! Construction failures of the shared analysis infrastructure.
//!
//! `Result`, not `Option`, and not `assert!`. This is library surface: a
//! caller who passes a hop wider than its window deserves to know which
//! invariant broke, and a caller feeding a short buffer deserves better than a
//! panic on the audio path.

use tutti_types::Samples;

use crate::window::{CosineWindow, Window};

/// What the STFT geometry and the grid refuse at construction.
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
}

/// Result of a geometry or grid call, defaulting the error to [`Error`].
pub type Result<T> = core::result::Result<T, Error>;
