//! The render stage: a `Net`, pulled one block at a time.
//!
//! - [`plan::RenderPlan`] — frame counts, derived once.
//! - [`driver::NetSource`] — the graph as a [`driver::FrameSource`].
//! - [`driver::Frames`] — interleaved samples that know their own width.
//! - [`driver::drive`] — the pull loop, applying the [`sink::BlockCursor`] gate.
//!
//! There is no sink type here. The *encoder* owns the pull (see
//! [`crate::encode`]): FLAC's library is itself pull-based, and giving every
//! format the same shape is what keeps buffering decorators unnecessary.

pub(crate) mod driver;
pub(crate) mod plan;
pub(crate) mod sink;

pub(crate) use driver::{drive, FrameSource, Frames, NetSource, PlaneSource};
pub(crate) use plan::RenderPlan;
pub(crate) use sink::BlockCursor;
