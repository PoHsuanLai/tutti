//! [`ClockNode`]: outbound MIDI Beat Clock and MTC as a graph node, out of
//! one event port.
//!
//! It ticks its [`ClockMaster`] once per transport segment of each block
//! (`Env::segments`), so a start, a stop or a locate inside a block sends its
//! Start, Stop or Song Position on its own frame, and the ticks after it
//! follow the transport from there. Tap its port
//! (`Graph::open_event_tap`) for a hardware pump to drain.
//!
//! The master is shared: the node's controls are the same `Arc<ClockMaster>`,
//! through which a control thread enables it and picks MTC. `prepare` gives
//! it the graph's rate.
//!
//! Its fork sends nothing: an offline render drives no external gear.

use std::sync::Arc;
use tutti_graph::UmpAt;

use tutti_graph::{
    Cx, Event, ForkCause, ForkMode, ForkSource, Forked, IntoNode, Io, Node, NodeParts, Offset,
    Prepare, Shape, Status,
};
use tutti_rt::RtVec;
use tutti_types::SampleRate;

use super::clock_master::ClockMaster;
use crate::live::order::{in_offset_order, offset_of};

/// The most events a [`ClockNode`] sends in one block, its declared event
/// capacity. At 300 BPM and 30 fps an 8 192-frame block at 44.1 kHz holds
/// about 23 ticks and 23 quarter-frames.
///
/// A loop wrap in the block adds a Song Position, a tick on the loop's start
/// when it is on one and the pieces its restarted grid sends, so a short loop
/// in a long block adds that per wrap. A loop of a sixteenth of a beat (1 500
/// frames at 120 BPM and 48 kHz, 600 at 300 BPM) wraps about 14 times in an
/// 8 192-frame block, well under the cap; it takes a loop of about sixty
/// frames (a millisecond) to reach it. Past it the events of later passes are refused and
/// counted ([`ClockMaster::dropped_events`]).
pub const CLOCK_EVENT_CAPACITY: usize = 256;

/// Outbound MIDI clock and timecode as a graph node with one event output.
/// See the module docs.
///
/// ```
/// use tutti_types::{SampleRate, Samples};
/// use tutti_graph::NodeKey;
/// use tutti_graph::{Editor, Prepare};
/// use tutti_midi_runtime::ClockNode;
///
/// let (mut editor, _exec) = Editor::new(Prepare::new(SampleRate(48_000.0), MaxBlock::of(Samples(512))));
/// let clock = editor.insert(NodeKey(1), "clock", ClockNode::new());
/// clock.set_enabled(true);
/// ```
#[derive(Debug)]
pub struct ClockNode {
    master: Arc<ClockMaster>,
    /// Where a block's events are gathered, to go out in offset order. Capped
    /// at the port's capacity: past it an event is refused and counted on the
    /// master ([`ClockMaster::dropped_events`]), never grown into.
    pending: RtVec<UmpAt, CLOCK_EVENT_CAPACITY>,
}

impl ClockNode {
    /// A node with a disabled master; `prepare` sets its rate.
    #[must_use]
    pub fn new() -> Self {
        Self {
            master: Arc::new(ClockMaster::new(SampleRate(0.0))),
            pending: RtVec::new(),
        }
    }
}

impl Default for ClockNode {
    fn default() -> Self {
        Self::new()
    }
}

impl Node for ClockNode {
    /// No audio; one event output of [`CLOCK_EVENT_CAPACITY`].
    fn shape(&self) -> Shape {
        Shape::EMPTY
            .with_events(0, 1)
            .with_event_capacity(CLOCK_EVENT_CAPACITY as u32)
            // Event-only: a few pulses a block, nothing per frame (measured
            // about 0.3 µs a block at any length; a gain's 256 frames, 0.2).
            .with_cost(tutti_graph::Cost::Light)
            .with_no_note_events()
    }

    fn prepare(&mut self, p: &Prepare) {
        self.master.set_sample_rate(p.sample_rate());
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let block_len = cx.env.block_len;
        self.pending.clear();
        let pending = &mut self.pending;
        // A segment's events lie inside it, so inside the block; the clamp is
        // what the sort's `offset_of` did to a stray one.
        let last = block_len.get().saturating_sub(1);
        let mut dropped = 0u64;
        for (start, seg) in cx.env.segments() {
            let base = start.get();
            self.master
                .tick(&seg.transport, seg.block_len, &mut |(at, ump): UmpAt| {
                    // Never grows on the audio thread: past the capacity the
                    // rest are refused, as the port would refuse them, and
                    // counted for the host.
                    let at = base as usize + at.index();
                    let at = Offset::new(at.min(last), block_len).unwrap_or(Offset::ZERO);
                    if !pending.push((at, ump)) {
                        dropped += 1;
                    }
                });
        }
        self.master.count_dropped(dropped);
        in_offset_order(self.pending.as_mut_slice());
        let out = io.event_out(0);
        for e in &self.pending {
            let _ = out.push(Event::midi(offset_of(e, block_len), e.1));
        }
        Status::Modified
    }

    fn reset(&mut self) {}
}

/// A [`ClockNode`]'s fork: a node over a master of its own, disabled, that
/// nothing can reach to enable.
struct SilentClock;

impl ForkSource for SilentClock {
    fn fork(&self, _mode: ForkMode) -> Result<Forked, ForkCause> {
        Ok(Forked::new(Box::new(ClockNode::new())))
    }
}

impl IntoNode for ClockNode {
    /// The shared master: enable it, pick MTC and its frame rate.
    type Controls = Arc<ClockMaster>;

    fn into_parts(self) -> NodeParts<Arc<ClockMaster>> {
        let controls = Arc::clone(&self.master);
        NodeParts {
            node: Box::new(self),
            controls,
            fork: Some(Box::new(SilentClock)),
        }
    }
}
