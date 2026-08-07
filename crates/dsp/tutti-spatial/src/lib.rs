//! Spatial audio: placing a source in a speaker field, and rendering that
//! placement to speakers or to headphones.
//!
//! This is the engine's only **geometry** — azimuth, elevation, speaker layouts,
//! HRIR spheres. Everything a signal-processing node does per channel lives in
//! [`tutti_units`]; everything that needs to know *where a sound is* lives here.
//! That line is why the crate exists, and it is the one `tutti-units`' own
//! `mix_bus` comment already drew from the other side: summing `K` sources of
//! `N` channels is arity arithmetic with no geometry in it, so it stayed there
//! rather than paying a VBAP dependency to add two stereo signals.
//!
//! The dependency on `tutti-units` runs in the direction that reading suggests:
//! [`build_surround_mix`] assembles its graph out of general-purpose units
//! (`ChannelSumUnit` to fold the panners, `SvfFilterNode` to low-pass the LFE
//! send). Geometry consumes signal processing; nothing here is consumed back.
//!
//! # The live-value rule applies here too
//!
//! Every node in this crate is an `AudioUnit`, so `tutti_units`' mandatory rule
//! about live control values holds unchanged: a value a user can change while
//! the node renders lives behind an `Arc` and its setter takes `&self`. See the
//! `tutti_units` crate docs for the mechanism and why the failure is silent.

mod error;
pub use error::{Error, Result};

mod node_id;

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

pub use mix::{build_surround_mix, SurroundSource};
pub use nodes::SpatialPannerNode;

#[cfg(feature = "hrtf")]
pub use hrtf_node::HrtfBinauralNode;
#[cfg(feature = "hrtf")]
pub use hrtf_panner::HrtfBinauralError;
