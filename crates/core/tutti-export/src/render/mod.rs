//! The render stage: a graph, pulled one block at a time.
//!
//! - [`plan::RenderPlan`] — frame counts, derived once.
//! - [`driver::NetSource`] and [`driver::GraphSource`] — a `Net` or a native
//!   graph as a [`driver::FrameSource`]; [`driver::with_source`] picks one.
//! - [`driver::Frames`] — interleaved samples that know their own width.
//! - [`driver::drive`] — the pull loop, applying the [`sink::BlockCursor`] gate.
//!
//! There is no sink type here. The *encoder* owns the pull (see
//! [`crate::encode`]): FLAC's library is itself pull-based, and giving every
//! format the same shape is what keeps buffering decorators unnecessary.

pub(crate) mod driver;
pub(crate) mod plan;
pub(crate) mod sink;

// `Frames` is only named by the codec-gated encode arms; the rest are used by
// the render path, which is available whether or not a format can be written.
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) use driver::Frames;
pub(crate) use driver::{drive, with_source, FrameSource, PlaneSource};
pub(crate) use plan::RenderPlan;
pub(crate) use sink::BlockCursor;
