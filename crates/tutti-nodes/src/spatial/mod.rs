//! Spatial panners.
//!
//! - [`VbapPannerNode`]: a stereo-in, N-out loudspeaker panner.
//! - [`VbapError`]: what building a VBAP panner can fail with.
//! - [`HrtfBinauralNode`]: the binaural renderer.
//!   Its error, `tutti_dsp::HrtfBinauralError`, and the sphere rates
//!   (`MIN_SPHERE_RATE`, `MAX_SPHERE_RATE`, `MAX_PREPARED_RATE`) are
//!   `tutti-dsp`'s.
//!
//! A mix of several panned sources is plain fan-in: wire each panner's
//! outputs to the same graph ports and the graph sums them.

mod hrtf;
mod target;
mod vbap;

pub(crate) use target::AngleSmoother;

pub use hrtf::{HRTF_BINAURAL_PARAMS, HrtfBinauralNode};
pub use vbap::{VBAP_PANNER_PARAMS, VbapError, VbapPannerNode};
