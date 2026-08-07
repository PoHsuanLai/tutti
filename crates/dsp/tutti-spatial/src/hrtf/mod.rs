//! Binaural rendering by FFT convolution against a measured HRIR sphere.
//! Always 2 channels out; no speaker layout involved.
//!
//! There is no `build_binaural_mix` counterpart to
//! [`build_vbap_mix`](crate::vbap::build_vbap_mix): summing binaural
//! renders is plain stereo addition, which `tutti_units::ChannelSumUnit`
//! already does.

mod node;
pub(crate) mod panner;

pub use node::HrtfBinauralNode;
pub use panner::HrtfBinauralError;
