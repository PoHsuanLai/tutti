pub mod types;
pub use types::ChannelLayout;

// Position-smoothing primitives (ExponentialSmoother) used by the panner nodes.
// Lives here because spatial is the only consumer.
mod smoothing;

mod binaural_panner;
mod nodes;
mod vbap_panner;

pub use nodes::{BinauralPannerNode, SpatialPannerNode};
