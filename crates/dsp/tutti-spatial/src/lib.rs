//! Spatial audio: VBAP speaker panning and HRTF binaural rendering.
//!
//! Two independent renderers, each named for its algorithm:
//!
//! | | [`vbap`] | `hrtf` |
//! |---|---|---|
//! | renders to | loudspeakers | headphones |
//! | outputs | `layout.count()` | always 2 |
//! | needs | a speaker layout | an HRIR dataset |
//! | fails with | [`vbap::VbapError`] | `hrtf::HrtfBinauralError` |
//!
//! Each owns its own error type; there is no crate-level `Error`.
//!
//! The `hrtf` module is behind the `hrtf` feature, which is off by default —
//! that column is absent from a default build, which is why its links are plain
//! backticks.
//!
//! Shared between them: [`SpatialTarget`] (bearing/height as lock-free params),
//! the position de-zipper in `smoothing`/`target`, and `layout` (SMPTE/WAV
//! channel order — a property of the destination buffer, not of either panner).
//!
//! # Angles do not compare or add
//!
//! [`Azimuth`](tutti_core::Azimuth) and [`Elevation`](tutti_core::Elevation)
//! carry no `Ord` and no `Add`: a circle has no ends, so "greater" is undefined
//! and a sum has no origin. Aiming at -170° from 170° is a 20° move across the
//! seam, not a 340° sweep back through zero — take differences in the scalar
//! space via `.get()`, and let [`SpatialTarget::store`] normalize (the bearing
//! wraps, the height clamps).

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
