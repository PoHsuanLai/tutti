//! Real-time audio-thread primitives.
//!
//! - [`scratch`] — [`RtScratch`](scratch::RtScratch), a fixed-capacity scratch
//!   buffer the audio callback borrows without allocating.
//! - [`denormals`] — [`ScopedNoDenormals`](denormals::ScopedNoDenormals), an
//!   RAII guard that flushes denormalized floats to zero for the callback's
//!   duration.

pub mod denormals;
pub mod scratch;

pub use denormals::ScopedNoDenormals;
pub use scratch::{RtScratch, RtScratchOverflow};
