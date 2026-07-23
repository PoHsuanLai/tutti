//! Offline rendering stage.
//!
//! Drives a `tutti_core::dsp::Net` block-by-block, gates each block, and pushes
//! the kept stereo frames into an [`AudioOut`] sink. Composed of three
//! children:
//!
//! - [`plan::RenderPlan`] — derive sample counts.
//! - [`driver::drive`] — the single block loop (owns the [`BlockCursor`] gate).
//! - [`sink`] — the [`AudioOut`] block consumers ([`RenderOut`]/[`StreamOut`]).
//!
//! The public entry [`render`] is pure composition over those three.

pub(crate) mod driver;
pub(crate) mod plan;
pub(crate) mod sink;

pub(crate) use plan::RenderPlan;
pub(crate) use sink::{BlockCursor, RenderOut, StreamOut};
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) use sink::{BufferingOut, Mastering};

use crate::progress::ProgressEmitter;
use crate::Result;
use std::sync::Arc;
use tutti_core::io::AudioOut;
use tutti_core::transport::OfflineTimeline;

/// All the inputs needed to render one offline pass. Separating the
/// specification (this struct) from the consumer interface (`sink`,
/// `progress`) keeps [`render`]'s body two function calls wide.
pub(crate) struct RenderRequest<'a> {
    pub net: tutti_core::dsp::Net,
    pub sample_rate: f64,
    pub duration_seconds: f64,
    pub compensate_latency: bool,
    /// Optional offline transport whose timeline advances with each block.
    pub timeline: Option<&'a Arc<OfflineTimeline>>,
}

/// Run one offline render: derive a plan from `request`, drive the net,
/// and pump every block into `sink`.
pub(crate) fn render(
    request: RenderRequest<'_>,
    sink: &mut dyn AudioOut,
    progress: &mut ProgressEmitter<'_>,
) -> Result<()> {
    let mut net = request.net;
    let plan = RenderPlan::new(
        &mut net,
        request.sample_rate,
        request.duration_seconds,
        request.compensate_latency,
    );
    driver::drive(
        &mut net,
        request.sample_rate,
        &plan,
        request.timeline,
        sink,
        progress,
    )
}
