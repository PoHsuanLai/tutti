//! Spatial audio: VBAP speaker panning and HRTF binaural rendering.
//!
//! Two independent renderers, each named for its algorithm:
//!
//! | | [`vbap`] | [`hrtf`] |
//! |---|---|---|
//! | renders to | loudspeakers | headphones |
//! | outputs | `layout.count()` | always 2 |
//! | needs | a speaker layout | an HRIR dataset |
//! | fails with | [`vbap::VbapError`] | [`hrtf::HrtfBinauralError`] |
//!
//! Each owns its own error type; there is no crate-level `Error`.
//!
//! Shared between them: [`SpatialTarget`] (bearing/height as lock-free params),
//! the position de-zipper in `smoothing`/`target`, and `layout` (SMPTE/WAV
//! channel order — a property of the destination buffer, not of either panner).

mod layout;
mod node_id;
mod smoothing;
mod target;

pub mod vbap;

#[cfg(feature = "hrtf")]
pub mod hrtf;

pub(crate) use target::AngleSmoother;
pub use target::SpatialTarget;

pub use vbap::{build_vbap_mix, VbapError, VbapPannerNode, VbapSource};

#[cfg(feature = "hrtf")]
pub use hrtf::{HrtfBinauralError, HrtfBinauralNode};
