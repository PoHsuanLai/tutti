//! The render stage: a graph, pulled one block at a time.
//!
//! - [`plan::RenderPlan`] — frame counts, derived once.
//! - [`driver::GraphSource`] — a graph as a block source of planes, built by
//!   [`driver::with_source`].
//! - [`driver::drive`] — the pull loop, applying the [`sink::BlockCursor`] gate.
//! - [`outputs`] — the outputs one pass feeds, and the passes after it
//!   (measure, write, finalize).
//!
//! There is no sink type here. The pipeline owns the pull (see
//! [`crate::encode`]) and hands each block to a `tutti_codec::Encoder`, which
//! only writes what it is given.

pub(crate) mod driver;
pub(crate) mod outputs;
pub(crate) mod plan;
pub(crate) mod sink;
pub(crate) use driver::{
    FileChannels, PlaneSource, REPLAY_BLOCK, Replay, check_loss, drive, with_source,
};
pub(crate) use plan::RenderPlan;
pub(crate) use sink::BlockCursor;
