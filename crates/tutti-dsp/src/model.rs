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
mod system;

pub use cadence::{Anchor, Cadence, Period};
pub use ceiling::{Ceiling, MaxDecay, MaxDecayError, Range};
pub use design::{Channels, Design, Sizes};
pub use lti::Lti;
pub use realization::{Effect, Effort, Realization, Setup};
pub use system::{Ring, System};
