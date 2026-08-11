//! Vector Base Amplitude Panning: per-speaker gains from a bearing and height,
//! so a source lands between the speakers nearest its direction.
//!
//! [`build_vbap_mix`] assembles the whole `sources → panners → sum` graph,
//! including LFE bass management. Named for the algorithm rather than the output
//! shape: it calls [`VbapPannerNode::for_layout`], so there is no non-VBAP way
//! to reach it.
//!
//! # Why this is a module and not a `SpatialPanner` variant
//!
//! The split from `hrtf` is not a taxonomy of "speakers vs headphones" — it is
//! that the two renderers are parameterized by different things and fail in
//! different ways, so no single type can carry both without one half of its
//! surface being inert.
//!
//! A VBAP panner is parameterized by a **speaker layout**: its output width is
//! `layout.count()`, and construction is fallible because a width outside the
//! preset set has no defined arrangement
//! ([`VbapError::UnsupportedSpeakerLayout`]). An HRTF renderer is parameterized
//! by an **HRIR dataset**: its output is always 2, and it fails on a dataset
//! that cannot be decoded — a disjoint condition with a disjoint error type.
//!
//! Unifying them behind one enum or trait would mean a constructor taking both
//! a layout and a dataset with each ignored by one arm, and an output width
//! that is sometimes a function of an argument and sometimes the constant 2.
//! Two modules, two error types, and no crate-level `Error` is the cheaper
//! shape: a caller has already chosen its destination before it reaches either,
//! and that choice is what selects the module.
//!
//! [`VbapError::UnsupportedSpeakerLayout`]: VbapError::UnsupportedSpeakerLayout

mod error;
mod mix;
mod node;
mod panner;

pub use error::{Result, VbapError};
pub use mix::{build_vbap_mix, VbapSource};
pub use node::VbapPannerNode;
