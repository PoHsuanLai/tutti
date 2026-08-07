//! Vector Base Amplitude Panning: per-speaker gains from a bearing and height,
//! so a source lands between the speakers nearest its direction.
//!
//! [`build_vbap_mix`] assembles the whole `sources → panners → sum` graph,
//! including LFE bass management. Named for the algorithm rather than the output
//! shape: it calls [`VbapPannerNode::for_layout`], so there is no non-VBAP way
//! to reach it.

mod error;
mod mix;
mod node;
mod panner;

pub use error::{Result, VbapError};
pub use mix::{build_vbap_mix, VbapSource};
pub use node::VbapPannerNode;
