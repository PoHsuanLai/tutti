//! Vector base amplitude panning: the speaker layout and the gain solve.
//!
//! A [`SpeakerLayout`] is built once, on the control side, from the speakers'
//! positions: it picks the **pairs** (a horizontal layout) or **triplets** (a
//! layout with height) of speakers a source can be placed between and keeps
//! the inverse of each tuple's direction matrix. [`SpeakerLayout::gains`] then
//! turns a bearing, a height and a spread into the speaker gains with no
//! allocation, every control passed per call.
//!
//! The mathematics is Pulkki's (1997): a source direction `p` is the sum of
//! the tuple's speaker directions weighted by gains, `g = L^-1 p`, the tuple
//! whose smallest gain is largest is the one the source lies inside, and the
//! gains are scaled to unit power. The tuples themselves are chosen from
//! Pulkki's published method, not from any implementation of it: see
//! `select.rs`.

mod error;
mod layout;
mod math;
mod select;

pub use error::SpeakerGeometryError;
pub use layout::{MAX_SPEAKERS, PanningMode, SpeakerLayout, Tuple};

#[cfg(test)]
mod tests;
