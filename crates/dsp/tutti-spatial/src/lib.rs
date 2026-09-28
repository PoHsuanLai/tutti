#![doc = include_str!("../README.md")]
//!
//! ## Items
//!
//! - [`VbapPannerNode`] and its [`VbapPannerControls`]: a stereo-in, N-out
//!   loudspeaker panner.
//! - [`build_vbap_mix`] and [`vbap_mix_parts`]: a whole multi-source VBAP mix,
//!   with [`VbapSource`], [`VbapLfeSend`], [`VbapMixParts`], [`VbapMixNode`]
//!   and [`VbapMixEdge`].
//! - [`SpatialTarget`]: a source's bearing and height as lock-free cells.
//! - [`VbapError`]: what building a VBAP panner or mix can fail with; the
//!   [`vbap`] module also holds its `Result` alias.
//! - `HrtfBinauralNode`, `HrtfBinauralControls` and `HrtfBinauralError`: the
//!   binaural renderer, behind the `hrtf` feature.

mod fork;
mod layout;
mod smoothing;
mod target;

pub mod vbap;

#[cfg(feature = "hrtf")]
mod hrtf;

pub(crate) use target::AngleSmoother;
pub use target::SpatialTarget;

pub use vbap::{
    build_vbap_mix, vbap_mix_parts, VbapError, VbapLfeSend, VbapMixEdge, VbapMixNode, VbapMixParts,
    VbapPannerControls, VbapPannerNode, VbapSource,
};

#[cfg(feature = "hrtf")]
pub use hrtf::{HrtfBinauralControls, HrtfBinauralError, HrtfBinauralNode};
