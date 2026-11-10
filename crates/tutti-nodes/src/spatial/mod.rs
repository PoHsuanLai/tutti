//! Spatial panners.
//!
//! - [`VbapPannerNode`] and its [`VbapPannerControls`]: a stereo-in, N-out
//!   loudspeaker panner.
//! - [`SpatialTarget`]: a source's bearing and height as lock-free cells.
//! - [`VbapError`]: what building a VBAP panner can fail with.
//! - [`HrtfBinauralNode`] and [`HrtfBinauralControls`]: the binaural renderer.
//!   Its error, `tutti_dsp::HrtfBinauralError`, and the sphere rates
//!   (`MIN_SPHERE_RATE`, `MAX_SPHERE_RATE`, `MAX_PREPARED_RATE`) are
//!   `tutti-dsp`'s.
//!
//! A mix of several panned sources is plain fan-in: wire each panner's
//! outputs to the same graph ports and the graph sums them.

mod fork;
mod hrtf;
mod layout;
mod target;
mod vbap;

pub(crate) use target::AngleSmoother;
pub use target::SpatialTarget;

pub use hrtf::{HrtfBinauralControls, HrtfBinauralNode};
pub use vbap::{VbapError, VbapPannerControls, VbapPannerNode};
