//! [`MidiOutNode`]: MIDI leaving the graph for a control thread (a hardware
//! MIDI-out pump), in at one event port.
//!
//! Each block it pushes the MIDI arriving at its event input into a ring
//! whose [`MidiReceiver`] its controls hold, every event's `frame_offset`
//! stamped with its frame on the graph's clock (`Env::frame` plus its offset,
//! wrapping at 2³², about 25 hours at 48 kHz): a reader on another thread
//! sees when each was due relative to the others, whatever the blocks were.
//! A full ring drops the rest and counts them
//! ([`MidiOutControls::dropped`]). Wire any MIDI event output to it: a clip,
//! a hosted plugin's MIDI out, the [`ClockNode`](crate::ClockNode).
//!
//! Its fork discards: an offline render sends nothing to a wire.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tutti_core::ChannelLayout;
use tutti_graph::{
    Cx, EventKind, ForkCause, ForkMode, ForkSource, Forked, IntoNode, Io, Node, NodeParts, Prepare,
    Shape, Status, Ump,
};
use tutti_midi_types::ump::MidiEvent;

use super::mailbox::{MidiMailbox, MidiReceiver, MidiSender};

/// A [`MidiOutNode`]'s ring, in events.
pub const MIDI_OUT_CAPACITY: usize = 1024;

/// A graph sink sending its event input's MIDI to a control thread. See the
/// module docs.
///
/// ```
/// use tutti_core::{NodeKey, SampleRate, Samples};
/// use tutti_graph::{Editor, Prepare};
/// use tutti_midi_runtime::MidiOutNode;
///
/// let (mut editor, _exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
/// let out = editor.insert(NodeKey(1), "midi out", MidiOutNode::new());
/// assert_eq!(out.dropped(), 0);
/// ```
pub struct MidiOutNode {
    tx: Option<MidiSender>,
    parts: Option<MidiOutControls>,
    dropped: Arc<AtomicU64>,
}

impl MidiOutNode {
    /// A node with an empty ring of [`MIDI_OUT_CAPACITY`] events.
    pub fn new() -> Self {
        let (tx, rx) = MidiMailbox::with_capacity(MIDI_OUT_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        Self {
            tx: Some(tx),
            parts: Some(MidiOutControls {
                rx: Arc::new(rx),
                dropped: Arc::clone(&dropped),
            }),
            dropped,
        }
    }

    /// A node that discards what arrives: the fork.
    fn discarding() -> Self {
        Self {
            tx: None,
            parts: None,
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl Default for MidiOutNode {
    fn default() -> Self {
        Self::new()
    }
}

/// The host's handle on a [`MidiOutNode`]: the ring's pop half. Cheap to
/// clone; one clone at a time should poll it.
#[derive(Clone)]
pub struct MidiOutControls {
    rx: Arc<MidiReceiver>,
    dropped: Arc<AtomicU64>,
}

impl MidiOutControls {
    /// Pop up to `out.len()` events, oldest first; return how many. Each
    /// event's `frame_offset` is its frame on the graph's clock, wrapping at
    /// 2³² (see the module docs).
    pub fn poll_into(&self, out: &mut [MidiEvent]) -> usize {
        self.rx.poll_into(out)
    }

    /// The ring's pop half.
    pub fn receiver(&self) -> &MidiReceiver {
        &self.rx
    }

    /// Events dropped because the ring was full, since the node was made.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Node for MidiOutNode {
    /// No audio; one event input.
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(1, 0)
    }

    fn prepare(&mut self, _p: &Prepare) {}

    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        let Some(tx) = &self.tx else {
            return Status::Modified;
        };
        let block = cx.env.frame.get();
        for e in io.events(0) {
            if let EventKind::Midi(Ump(data)) = e.kind {
                // Truncated on purpose: the frame wraps at 2^32.
                let at = block.wrapping_add(u64::from(e.offset.get())) as u32;
                let ev = MidiEvent {
                    frame_offset: at,
                    data,
                };
                if tx.queue(&[ev]) == 0 {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {}
}

/// A [`MidiOutNode`]'s fork: one that discards.
struct Discarding;

impl ForkSource for Discarding {
    fn fork(&self, _mode: ForkMode<'_>) -> Result<Forked, ForkCause> {
        Ok(Forked::new(Box::new(MidiOutNode::discarding())))
    }
}

impl IntoNode for MidiOutNode {
    type Controls = MidiOutControls;

    fn into_parts(mut self) -> NodeParts<MidiOutControls> {
        let controls = self.parts.take().expect("a new node has its controls");
        NodeParts {
            node: Box::new(self),
            controls,
            fork: Some(Box::new(Discarding)),
        }
    }
}
