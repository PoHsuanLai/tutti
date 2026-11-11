//! The neutral model module (design 024): the traits every family of processor
//! answers to, and the engineering that sizes and schedules what runs it.
//!
//! A [`Design`] is a parameter map, a [`System`] (and its class, [`Lti`]) is the
//! mathematical object it gives, and a [`Realization`] is one structure that
//! computes it. [`Ceiling`], [`Sizes`], [`Effort`] and [`Cadence`] are the
//! engineering layer: none is a property of the system.

mod cadence;
mod ceiling;
mod design;
mod lti;
mod realization;
mod realized;
mod system;

pub use cadence::{Anchor, Cadence, Period, Step};
pub use ceiling::{Ceiling, MaxDecay, MaxDecayError, Range};
pub use design::{Channels, Design, Placement, Sizes};
pub use lti::Lti;
pub use realization::{Effect, Effort, Realization, Setup};
#[cfg(test)]
pub(crate) use realized::Flaw;
pub use realized::{Approach, Glide, Kernel, Lerp, Realized, Steps};
pub use system::{Ring, System};
