//! Musical time: where the playhead is, how fast it moves, and what moves it.
//!
//! Split by *who decides*. [`TransportSettings`] holds the plain values (tempo,
//! loop region, recording) that anyone may store into; [`MotionFsm`] holds the
//! state machine that may reject or defer a play/stop/locate. [`Transport`] is
//! the two halves as one handle, and [`TransportAudio`] holds the engine's
//! playhead, which the graph reads from each block's `Env`.
//!
//! The traits below are the read side: [`Timeline`] is what a reader outside
//! the graph consults (a node reads its block's `Env`)
//! and [`RenderClock`] is what a renderer drives.
//!
//! `tutti-engine`'s `Engine` holds a [`TransportAudio`], the audio-thread side
//! of a [`Transport`] (taken once): the clock, the motion machine and the
//! timed commands behind one value that private fields keep in order.

#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod audio;
#[cfg(test)]
mod beat_walk;
mod clock;
mod fsm;
mod handle;
mod loop_region;
mod motion;
mod offline;
mod project_meter;
mod settings;
mod state;
mod timed;
mod timeline;
mod walk;

pub use audio::{Applied, AudioTaken, TransportAudio};
pub use handle::Transport;
pub use loop_region::{LoopEdit, LoopRegion};
pub use motion::{FadeOut, MotionEvent, MotionFsm, MotionState, QueueFull, Then};
pub use offline::{OfflineTimeline, OfflineTimelineConfig};
pub use project_meter::{MeterSlot, MeterSlotCell, MeterSlotReader, ProjectMeter};
pub use settings::TransportSettings;
pub use timed::{IMMEDIATE_RESERVE, SCHEDULE_CAPACITY, ScheduleFull, Scheduled, TransportCommand};

pub use timeline::{OfflineClock, OfflineTransport, Timeline};
pub use walk::TempoAhead;

/// A clock an offline render drives, one block at a time.
///
/// [`Timeline`] is read-only — `beat`/`tempo`/`is_rolling` are what a reader
/// consults. Something has to move that position forward, and in a live session
/// it is the audio callback. Offline there is no callback, so the renderer does
/// it: after each block it reports how many frames it produced, and the clock
/// advances by exactly that much.
///
/// A renderer driving the graph (`tutti_graph::Executor`) must also *hand*
/// each block a transport, since
/// the graph's clock is the executor's `Env`, not a node — that is
/// [`graph_block`](Self::graph_block), and [`render_graph`](Self::render_graph)
/// is the one order the two are called in. It is still not a supertrait of
/// `Timeline`: a clock that does not move (`FrozenClock`) has a transport to
/// report — stopped, at beat zero — without being a timeline anything reads.
///
/// **Advance AFTER processing, never before.** The engine's clock (whose
/// transport beat-driven nodes read from `Env`) is
/// emit-then-advance: sample 0 of a block carries the block's start beat, and
/// only then does the beat increment. A clock advanced ahead of the render
/// sits one `beats_per_sample` off the transport its nodes read for the
/// entire render — a desync that reads as "the samplers are
/// slightly late" and nothing else.
pub trait RenderClock: Send + Sync {
    /// Advances the playhead by `frames` — a **frame** count, not a sample
    /// count, so it is independent of the render's channel width.
    ///
    /// Call after the block has been processed, never before.
    fn advance(&self, frames: tutti_types::Samples);

    /// The block about to be rendered as the graph takes it: the
    /// transport at the block's first frame, and the changes inside it. Read
    /// **before** the block is processed, as [`render_graph`](Self::render_graph)
    /// does.
    ///
    /// Required, not defaulted. The obvious default — stopped, at beat zero —
    /// is right for a clock that does not move and wrong for every clock that
    /// does: a moving clock that fell back on it would advance its clip readers
    /// while every `Env` reader held beat zero, a
    /// desync that renders without an error. A clock that moves must say where
    /// it is.
    fn graph_block(&self) -> (tutti_graph::Transport, tutti_graph::TransportChanges);

    /// Renders `frames` through `exec` under this clock, then advance the
    /// clock by it: [`graph_block`](Self::graph_block),
    /// `Executor::process_with_changes`, then [`advance`](Self::advance) —
    /// the one order that keeps every reader of this clock on the frame the
    /// graph renders (emit-then-advance, as above). `frames` is one block:
    /// the executor never splits it, and a node reads a transport change
    /// inside it from its `Env`.
    ///
    /// # Panics
    ///
    /// As `Executor::process_with_changes`: if `frames` is zero or past the
    /// executor's prepared maximum block.
    fn render_graph(
        &self,
        exec: &mut tutti_graph::Executor,
        frames: usize,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
    ) {
        let (transport, changes) = self.graph_block();
        exec.process_with_changes(frames, &transport, &changes, inputs, outputs);
        self.advance(tutti_types::Samples(frames))
    }
}

/// A clock that does not move.
///
/// For rendering a graph with no time-dependent nodes — a synth patch, a test
/// tone, an impulse response. It exists so that "this graph has no transport"
/// is something a caller *states* rather than something they omit: a renderer
/// takes a clock, so forgetting one is a compile error instead of a silently
/// silent render.
#[derive(Debug, Clone, Copy, Default)]
pub struct FrozenClock;

impl RenderClock for FrozenClock {
    fn advance(&self, _frames: tutti_types::Samples) {}

    /// Stopped at beat zero, every block: "this graph has no transport".
    fn graph_block(&self) -> (tutti_graph::Transport, tutti_graph::TransportChanges) {
        (
            tutti_graph::Transport::default(),
            tutti_graph::TransportChanges::NONE,
        )
    }
}
