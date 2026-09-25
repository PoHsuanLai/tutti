//! Spatial audio: VBAP speaker panning and HRTF binaural rendering.
//!
//! Two independent renderers, each named for its algorithm rather than the
//! category it falls in: [`vbap`] (loudspeakers, `layout.count()` outputs,
//! failing with [`vbap::VbapError`]) and `hrtf` (headphones, always 2, failing
//! with `hrtf::HrtfBinauralError`). Each owns its own error type; there is no
//! crate-level `Error`.
//!
//! The `hrtf` module is behind the `hrtf` feature, which is off by default —
//! that renderer is absent from a default build, which is why its links here are
//! plain backticks.
//!
//! Shared between them: [`SpatialTarget`] (bearing and height as lock-free
//! params), the position de-zipper in `smoothing`/`target`, and `layout` (SMPTE
//! /WAV channel order — a property of the destination buffer, not of either
//! panner).
//!
//! A [`VbapPannerNode`] is **stereo-in**, N-out: it distributes a source across
//! the layout's speakers by bearing, and a mono source presents the same sample
//! on both input ports. Moving the source is a lock-free write, so it may happen
//! while the node renders — but see the `reset` deviation below before reaching
//! for that method to clear a tail.
//!
//! The renderer table, both quick starts, the angle-algebra constraint and the
//! `reset` deviation are in the crate README, included below.
#![doc = include_str!("../README.md")]

mod layout;
mod node_id;
mod smoothing;
mod target;

pub mod vbap;

#[cfg(feature = "hrtf")]
mod hrtf;

pub(crate) use target::AngleSmoother;
pub use target::SpatialTarget;

pub use vbap::{
    build_vbap_mix, vbap_mix_parts, VbapError, VbapLfeSend, VbapMixEdge, VbapMixNode, VbapMixParts,
    VbapPannerNode, VbapSource,
};

#[cfg(feature = "hrtf")]
pub use hrtf::{HrtfBinauralError, HrtfBinauralNode};
