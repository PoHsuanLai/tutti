// Position-smoothing primitives (ExponentialSmoother) used by the panner nodes.
// Lives here because spatial is the only consumer.
mod smoothing;

mod mix;
mod nodes;
mod vbap_panner;

#[cfg(feature = "hrtf")]
mod hrtf_node;
#[cfg(feature = "hrtf")]
mod hrtf_panner;

pub use mix::{build_surround_mix, ChannelSumUnit, SurroundSource};
pub use nodes::SpatialPannerNode;

#[cfg(feature = "hrtf")]
pub use hrtf_node::HrtfBinauralNode;
#[cfg(feature = "hrtf")]
pub use hrtf_panner::HrtfBinauralError;
