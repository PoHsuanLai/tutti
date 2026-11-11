//! [`VbapError`] and the module-local [`Result`] alias.
//!
//! Scoped to `vbap`. The binaural side owns `HrtfBinauralError`; there is no
//! crate-level error type to unify them, and adding one would claim a shared
//! failure mode that does not exist.

use thiserror::Error;
use tutti_types::ChannelTopology;

/// The error returned when a VBAP panner or mix cannot be built.
#[derive(Debug, Clone, Error)]
#[non_exhaustive]
pub enum VbapError {
    /// A topology that is none of the five beds (the engine's stereo, quad,
    /// 5.1, 7.1 and 7.1.4), so there are no speaker angles for it.
    #[error("no VBAP speaker bed for {0:?} (have stereo, quad, 5.1, 7.1 and 7.1.4)")]
    NoBed(ChannelTopology),
}

/// `Result` with [`VbapError`] as the failure type.
pub type Result<T> = core::result::Result<T, VbapError>;
