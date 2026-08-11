//! [`VbapError`] and the module-local [`Result`] alias.
//!
//! Scoped to `vbap` because both variants are speaker geometry. The binaural
//! side owns `HrtfBinauralError`; there is no crate-level error type to unify
//! them, and adding one would claim a shared failure mode that does not exist.

use thiserror::Error;

/// What VBAP panning can fail at. Module-scoped because both variants are
/// speaker geometry; HRTF has its own error type.
#[derive(Debug, Clone, Error)]
pub enum VbapError {
    /// The `vbap` crate rejected the speaker geometry — a degenerate preset
    /// whose speaker triplets do not span the sphere.
    #[error("VBAP error: {0:?}")]
    Vbap(vbap::VBAPError),

    /// A width with no speaker preset. Only 2/4/6/8/12 are defined.
    #[error("no speaker preset for a {0}-channel layout (have 2/4/6/8/12)")]
    UnsupportedSpeakerLayout(u16),
}

impl From<vbap::VBAPError> for VbapError {
    fn from(err: vbap::VBAPError) -> Self {
        Self::Vbap(err)
    }
}

/// Result of a VBAP operation, defaulting the error to [`VbapError`].
pub type Result<T> = core::result::Result<T, VbapError>;
