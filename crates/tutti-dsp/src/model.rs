//! The neutral model module: the traits every family of processor
//! answers to, and the engineering that sizes and schedules what runs it.
//!
//! A [`Family`] is a parameter map, a [`System`] (and its class, [`Lti`]) is the
//! mathematical object it gives (a member of the family), and a [`Recurrence`], run by
//! [`Realized`], is one structure that computes it. [`Grows`], [`Capacity`], [`Reach`],
//! [`Domain`], [`Effort`] and [`Cadence`] are the engineering layer: none is a property
//! of the system.

mod cadence;
mod capacity;
mod family;
mod feed;
mod lti;
mod reach;
mod realized;
mod setup;
mod system;

pub use cadence::{Cadence, Step};
pub use capacity::Capacity;
pub use family::{Channels, Family, Grows};
pub use feed::{Feed, MAX_LANES};
pub use lti::Lti;
pub use reach::{Domain, MaxDecay, MaxDecayError, Reach, TAIL_FLOOR};
pub use realized::{Approach, Glide, Lerp, Readout, Realized, Recurrence, Start, Steps};
pub use setup::{Effort, Setup};
pub use system::{Ring, System};
