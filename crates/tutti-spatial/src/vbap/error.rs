//! [`VbapError`] and the module-local [`Result`] alias.
//!
//! Scoped to `vbap` because both variants are speaker geometry. The binaural
//! side owns `HrtfBinauralError`; there is no crate-level error type to unify
//! them, and adding one would claim a shared failure mode that does not exist.

use thiserror::Error;
use tutti_dsp::SpeakerGeometryError;

/// The error returned when a VBAP panner or mix cannot be built.
///
/// Both variants are about speaker geometry; the binaural renderer has its own
/// error type.
#[derive(Debug, Clone, Error)]
#[non_exhaustive]
pub enum VbapError {
    /// The speaker geometry was rejected: a degenerate layout whose speakers
    /// form no pair or triplet.
    #[error("VBAP error: {0}")]
    Geometry(#[from] SpeakerGeometryError),

    /// A width with no speaker preset. Only 2/4/6/8/12 are defined.
    #[error("no speaker preset for a {0}-channel layout (have 2/4/6/8/12)")]
    UnsupportedSpeakerLayout(u16),
}

/// `Result` with [`VbapError`] as the failure type.
pub type Result<T> = core::result::Result<T, VbapError>;
