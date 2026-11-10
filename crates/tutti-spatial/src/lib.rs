#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]
//!
//! ## Items
//!
//! - [`VbapPannerNode`] and its [`VbapPannerControls`]: a stereo-in, N-out
//!   loudspeaker panner.
//! - [`build_vbap_mix`] and [`VbapMixParts::new`]: a whole multi-source VBAP mix,
//!   with [`VbapSource`], [`VbapLfeSend`], [`VbapMixParts`], [`VbapMixNode`]
//!   and [`VbapMixEdge`].
//! - [`SpatialTarget`]: a source's bearing and height as lock-free cells.
//! - [`VbapError`]: what building a VBAP panner or mix can fail with; the
//!   [`vbap`] module also holds its `Result` alias.
//! - `HrtfBinauralNode` and `HrtfBinauralControls`: the binaural renderer,
//!   behind the `hrtf` feature. Its error, `tutti_dsp::HrtfBinauralError`, and
//!   the sphere rates (`MIN_SPHERE_RATE`, `MAX_SPHERE_RATE`,
//!   `MAX_PREPARED_RATE`) are `tutti-dsp`'s.

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
    VbapError, VbapLfeSend, VbapMixEdge, VbapMixNode, VbapMixParts, VbapPannerControls,
    VbapPannerNode, VbapSource, build_vbap_mix,
};

#[cfg(feature = "hrtf")]
pub use hrtf::{HrtfBinauralControls, HrtfBinauralNode};
