//! [`ClockNode`]: outbound MIDI Beat Clock and MTC as a graph node, out of
//! one event port (doc 013, rewrite item 5).
//!
//! It ticks its [`ClockMaster`] once per transport segment of each block
//! (`Env::segments`), so a start, a stop or a locate inside a block sends its
//! Start, Stop or Song Position on its own frame, and the ticks after it
//! follow the transport from there. Wire its port to a
//! [`MidiOutNode`](crate::MidiOutNode) for a hardware pump to drain.
//!
//! The master is shared: the node's controls are the same `Arc<ClockMaster>`,
//! through which a control thread enables it and picks MTC. `prepare` gives
//! it the graph's rate.
//!
//! Its fork sends nothing: an offline render drives no external gear.

use std::sync::Arc;

use tutti_core::{ChannelLayout, SampleRate};
use tutti_graph::{
    Cx, Event, ForkCause, ForkMode, ForkSource, Forked, IntoNode, Io, Node, NodeParts, Prepare,
    Shape, Status,
};
use tutti_midi_types::ump::MidiEvent;

use super::clock_master::ClockMaster;
use crate::live::order::{in_offset_order, offset_of};

/// The most events a [`ClockNode`] sends in one block, its declared event
/// capacity. At 300 BPM and 30 fps an 8 192-frame block at 44.1 kHz holds
/// about 23 ticks and 23 quarter-frames.
pub const CLOCK_EVENT_CAPACITY: usize = 256;

/// Outbound MIDI clock and timecode as a graph node with one event output.
/// See the module docs.
///
/// ```
/// use tutti_core::{NodeKey, SampleRate, Samples};
/// use tutti_graph::{Editor, Prepare};
/// use tutti_midi_runtime::ClockNode;
///
/// let (mut editor, _exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
/// let clock = editor.insert(NodeKey(1), "clock", ClockNode::new());
/// clock.set_enabled(true);
/// ```
pub struct ClockNode {
    master: Arc<ClockMaster>,
    /// Where a block's events are gathered, to go out in offset order.
    pending: Vec<MidiEvent>,
}

impl ClockNode {
    /// A node with a disabled master; `prepare` sets its rate.
    pub fn new() -> Self {
        Self {
            master: Arc::new(ClockMaster::new(SampleRate(0.0))),
            pending: Vec::with_capacity(CLOCK_EVENT_CAPACITY),
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
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
            .with_events(0, 1)
            .with_event_capacity(CLOCK_EVENT_CAPACITY as u32)
    }

    fn prepare(&mut self, p: &Prepare) {
        self.master.set_sample_rate(p.sample_rate());
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let block_len = cx.env.block_len;
        self.pending.clear();
        let pending = &mut self.pending;
        for (start, seg) in cx.env.segments() {
            let base = start.get();
            self.master
                .tick(&seg.transport, seg.block_len.get(), &mut |e: MidiEvent| {
                    // Never grows on the audio thread: past the capacity the
                    // rest are dropped, as the port would refuse them.
                    if pending.len() < pending.capacity() {
                        pending.push(e.with_frame_offset(base + e.frame_offset));
                    }
                });
        }
        in_offset_order(&mut self.pending);
        let out = io.event_out(0);
        for e in &self.pending {
            let _ = out.push(Event::midi(offset_of(e, block_len), e.data));
        }
        Status::Modified
    }

    fn reset(&mut self) {}
}

/// A [`ClockNode`]'s fork: a node over a master of its own, disabled, that
/// nothing can reach to enable.
struct SilentClock;

impl ForkSource for SilentClock {
    fn fork(&self, _mode: ForkMode<'_>) -> Result<Forked, ForkCause> {
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
