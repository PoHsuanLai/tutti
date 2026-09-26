//! [`MidiQueueNode`]: MIDI a control thread sends (a keyboard, a preview,
//! an all-notes-off) into the graph, out of one event port (doc 013, rewrite
//! item 5).
//!
//! The control thread pushes into a [`MidiSender`] (the node's controls);
//! each block the node drains what arrived and sends it out, every event on
//! its `frame_offset` clamped into the block (what a control thread builds
//! has offset 0: the block's first frame). Wire the port to the node that
//! should hear it; to reach several, wire it to each.
//!
//! Its fork is silent: nothing a control thread sends reaches an offline
//! render.

use tutti_core::ChannelLayout;
use tutti_graph::{
    Cx, Event, ForkCause, ForkMode, ForkSource, Forked, IntoNode, Io, Node, NodeParts, Prepare,
    Shape, Status,
};
use tutti_midi_types::ump::MidiEvent;

use super::mailbox::{MidiMailbox, MidiReceiver, MidiSender, MAILBOX_CAPACITY};
use super::order::{in_offset_order, offset_of};

/// A [`MidiQueueNode`]'s ring, and its event port's declared capacity.
pub const MIDI_QUEUE_CAPACITY: usize = MAILBOX_CAPACITY;

/// MIDI from a control thread as a graph node with one event output. See the
/// module docs.
///
/// ```
/// use tutti_core::{NodeKey, SampleRate, Samples};
/// use tutti_midi_types::MidiChannel;
/// use tutti_graph::{Editor, Prepare};
/// use tutti_midi_runtime::MidiQueueNode;
///
/// let (mut editor, _exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
/// let keys = editor.insert(NodeKey(1), "keyboard", MidiQueueNode::new());
/// assert!(keys.note_on(MidiChannel::FIRST, 60, 100));
/// ```
pub struct MidiQueueNode {
    rx: MidiReceiver,
    tx: MidiSender,
    scratch: Box<[MidiEvent]>,
}

impl MidiQueueNode {
    /// A node with an empty ring of [`MIDI_QUEUE_CAPACITY`] events.
    pub fn new() -> Self {
        let (tx, rx) = MidiMailbox::with_capacity(MIDI_QUEUE_CAPACITY);
        Self {
            rx,
            tx,
            scratch: vec![MidiEvent::noop(); MIDI_QUEUE_CAPACITY].into_boxed_slice(),
        }
    }
}

impl Default for MidiQueueNode {
    fn default() -> Self {
        Self::new()
    }
}

impl Node for MidiQueueNode {
    /// No audio; one event output of [`MIDI_QUEUE_CAPACITY`].
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
            .with_events(0, 1)
            .with_event_capacity(MIDI_QUEUE_CAPACITY as u32)
    }

    fn prepare(&mut self, _p: &Prepare) {}

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let block_len = cx.env.block_len;
        let n = self.rx.poll_into(&mut self.scratch);
        let events = &mut self.scratch[..n];
        in_offset_order(events);
        let out = io.event_out(0);
        for ev in events.iter() {
            let _ = out.push(Event::midi(offset_of(ev, block_len), ev.data));
        }
        Status::Modified
    }

    /// Keeps what is queued: a note-off waiting must still arrive.
    fn reset(&mut self) {}
}

/// A [`MidiQueueNode`]'s fork: the same port, nothing queued, no sender.
struct SilentQueue;

impl ForkSource for SilentQueue {
    fn fork(&self, _mode: ForkMode<'_>) -> Result<Forked, ForkCause> {
        Ok(Forked::new(Box::new(MidiQueueNode::new())))
    }
}

impl IntoNode for MidiQueueNode {
    /// The push half of the node's ring.
    type Controls = MidiSender;

    fn into_parts(self) -> NodeParts<MidiSender> {
        let controls = self.tx.clone();
        NodeParts {
            node: Box::new(self),
            controls,
            fork: Some(Box::new(SilentQueue)),
        }
    }
}
