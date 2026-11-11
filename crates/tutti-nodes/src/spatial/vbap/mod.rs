//! Vector Base Amplitude Panning: per-speaker gains from a bearing and height,
//! so a source lands between the speakers nearest its direction.
//!
//! - [`VbapPannerNode`] pans one stereo (or mono) source across a speaker
//!   layout; its params ([`VBAP_PANNER_PARAMS`]) move it while it renders.
//! - [`VbapError`] is what building a panner can fail with.
//!
//! A VBAP panner is parameterized by a speaker bed: its output width is the
//! bed's, and construction fails for a topology with no bed
//! ([`VbapError::NoBed`]). The binaural renderer
//! (`HrtfBinauralNode`) is parameterized by an HRIR dataset instead, always
//! has two outputs and has its own error type.

mod error;
mod node;
mod panner;

pub use error::VbapError;
pub use node::{VBAP_PANNER_PARAMS, VbapPannerNode};
