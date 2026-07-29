//! What an offline render hands its nodes so they can re-seat themselves.
//!
//! Paired with [`AudioUnit::rebind_offline`](crate::AudioUnit::rebind_offline)
//! and [`PendingClone::isolate_for_offline`](crate::dsp::PendingClone::isolate_for_offline):
//! the clone is severed from live state, then every node is handed one of these
//! to re-point at.
//!
//! The context is passed to nodes as `&dyn Any` (see `rebind_offline`'s docs —
//! `fundsp-tutti` cannot name [`Timeline`]), so this type is the agreed shape on
//! both sides of that cast. Downcast it with
//! `ctx.downcast_ref::<OfflineContext>()`.

use std::sync::Arc;

use crate::params::{Beat, Bpm};
use crate::transport::Timeline;

/// The clock an offline render drives, plus where and how fast it starts.
///
/// A node re-points whatever transport it holds at [`Self::transport`]. Nodes
/// carrying their own internal clock (rather than an `Arc<dyn Timeline>`) use
/// [`Self::start_beat`] and [`Self::tempo`] to re-seat it — the clock is severed
/// by `isolate()` but keeps whatever beat the *live* playhead happened to be at,
/// so without this every beat-driven node (LFO, automation) would render from
/// the wrong position.
#[derive(Clone)]
pub struct OfflineContext {
    /// The timeline the renderer advances, one block at a time.
    pub transport: Arc<dyn Timeline>,
    /// Beat the render begins at.
    pub start_beat: Beat,
    /// Tempo the render runs at.
    pub tempo: Bpm,
}

impl OfflineContext {
    pub fn new(transport: Arc<dyn Timeline>, start_beat: impl Into<Beat>, tempo: impl Into<Bpm>) -> Self {
        Self {
            transport,
            start_beat: start_beat.into(),
            tempo: tempo.into(),
        }
    }
}

impl std::fmt::Debug for OfflineContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `dyn Timeline` is not Debug; report what it reads instead.
        f.debug_struct("OfflineContext")
            .field("start_beat", &self.start_beat)
            .field("tempo", &self.tempo)
            .field("transport_beat", &self.transport.beat())
            .finish()
    }
}
