//! Vector Base Amplitude Panning: per-speaker gains from a bearing and height,
//! so a source lands between the speakers nearest its direction.
//!
//! [`build_surround_mix`] is VBAP-specific by construction — it calls
//! [`VbapPannerNode::for_layout`], and a surround mix only means something for a
//! panner with a speaker field to mix across.

mod error;
mod mix;
mod node;
mod panner;

pub use error::{Result, VbapError};
pub use mix::{build_surround_mix, SurroundSource};
pub use node::VbapPannerNode;
