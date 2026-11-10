//! Spatial algorithms: vector base amplitude panning (the speaker layout and
//! the gain solve) and the HRTF binaural convolver (`hrtf.rs`, over the HRIR
//! sphere of `sphere.rs`).
//!
//! ## VBAP
//!
//!
//! A [`SpeakerLayout`] is built once, on the control side, from the speakers'
//! positions: it picks the **pairs** (a horizontal layout) or **triplets** (a
//! layout with height) of speakers a source can be placed between and keeps
//! the inverse of each tuple's direction matrix. [`SpeakerLayout::gains_into`] then
//! turns a bearing, a height and a spread into the speaker gains with no
//! allocation, every control passed per call. A [`VbapKernel`] holds a layout and
//! solves one block's [`VbapGains`] (the mono fold of a narrow width, two virtual
//! sources otherwise, the last solve kept), and [`VbapGains::render_channel`] ramps one
//! output channel between two of them.
//!
//! The mathematics is Pulkki's (1997): a source direction `p` is the sum of
//! the tuple's speaker directions weighted by gains, `g = L^-1 p`, the tuple
//! whose smallest gain is largest is the one the source lies inside, and the
//! gains are scaled to unit power. The tuples themselves are chosen from
//! Pulkki's published method, not from any implementation of it: see
//! `select.rs`.
//!
//! ## HRTF
//!
//! [`HrtfBinaural`] convolves a mono input with the HRIR pair blended for the
//! direction it is told to render, `HRTF_STEP` frames at a time. The caller
//! smooths the direction and hands it over per step.

mod error;
mod hrtf;
mod layout;
mod math;
mod select;
mod sphere;
mod vbap;

pub use error::SpeakerGeometryError;
pub use hrtf::{
    HRTF_LATENCY, HRTF_STEP, HrtfBinaural, HrtfBinauralError, MAX_PREPARED_RATE, MAX_SPHERE_RATE,
    MIN_SPHERE_RATE,
};
pub use layout::{MAX_SPEAKERS, PanningMode, SpeakerLayout, Tuple};
pub use vbap::{VbapGains, VbapKernel};

#[cfg(test)]
mod tests;
