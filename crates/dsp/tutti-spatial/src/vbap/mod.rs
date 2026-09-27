//! Vector Base Amplitude Panning: per-speaker gains from a bearing and height,
//! so a source lands between the speakers nearest its direction.
//!
//! - [`VbapPannerNode`] pans one stereo (or mono) source across a speaker
//!   layout; [`VbapPannerControls`] move it while it renders.
//! - [`build_vbap_mix`] assembles the whole `sources → panners → sum` graph,
//!   including LFE bass management, into a `tutti_graph::GraphBuilder`;
//!   [`vbap_mix_parts`] returns the same mix as owned nodes and edges, for a
//!   graph built another way.
//! - [`VbapError`] is what building a panner or mix can fail with, and
//!   [`Result`] its alias.
//!
//! A VBAP panner is parameterized by a speaker layout: its output width is
//! `layout.count()`, and construction fails for a width with no preset
//! ([`VbapError::UnsupportedSpeakerLayout`]). The binaural renderer
//! (`HrtfBinauralNode`, behind the `hrtf` feature) is parameterized by an HRIR
//! dataset instead, always has two outputs and has its own error type.

mod error;
mod mix;
mod node;
mod panner;

pub use error::{Result, VbapError};
pub use mix::{
    build_vbap_mix, vbap_mix_parts, VbapLfeSend, VbapMixEdge, VbapMixNode, VbapMixParts, VbapSource,
};
pub use node::{VbapPannerControls, VbapPannerNode};
