//! [`MidiInputNode`]: a MIDI input edge (the hardware ports, typically) as a
//! graph node, its events out of one event port per channel.
//!
//! Each block it polls its [`MidiIn`], assembles (N)RPN runs
//! ([`Midi1ToMidi2Translator`]) and rewrites classic MPE into native per-note
//! messages ([`MpeIngest`]), then sends every event out of the port for its
//! channel: port `c` carries channel `c`'s voice messages, and
//! [`CHANNELLESS_PORT`] everything with no channel (system, SysEx, Flex,
//! Stream, utility). **Routing is wiring**: a synth listening to channel 3
//! takes event edges from port 3 and the channelless port, one listening to
//! every channel from all seventeen ([`input_ports`]).
//!
//! Translation runs on every event, wired or not: both stages are stateful
//! assemblers, and skipping an event corrupts the ones after it.
//!
//! Its fork is silent: an offline render reads no wire.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tutti_graph::{
    Cx, Event, EventDensity, ForkCause, ForkMode, ForkSource, Forked, IntoNode, Io, Node,
    NodeParts, Prepare, Shape, Status,
};
use tutti_midi_types::mpe::MpeMode;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{Midi1ToMidi2Translator, MidiIn};
use tutti_types::{ArcLend, ChannelLayout, Reader};
use tutti_ump::{MidiChannel, MidiGroup};

use super::order::{in_offset_order, offset_of};
use crate::outbound::mpe_ingest::{MpeIngest, Translated};

/// The port that carries events with no channel: after the sixteen channel
/// ports.
pub const CHANNELLESS_PORT: usize = MidiChannel::COUNT as usize;

/// How many event outputs a [`MidiInputNode`] has: one per channel and
/// [`CHANNELLESS_PORT`].
pub const MIDI_INPUT_PORTS: usize = CHANNELLESS_PORT + 1;

/// The fewest events a [`MidiInputNode`] polls in one block, and each port's
/// declared event capacity at the shortest blocks.
pub const MIDI_INPUT_EVENT_CAPACITY: usize = 512;

/// The events per frame a [`MidiInputNode`] is sized for: one in eight
/// (6,000 a second at 48 kHz, where a 16-channel MPE controller sending a
/// bend, a pressure and a slide every 8 ms on each channel is 6,000), which
/// with its block is the node's poll and port capacity when that is more than
/// [`MIDI_INPUT_EVENT_CAPACITY`] (blocks past 4,096 frames). A hardware wire
/// carries far less than that, so this is headroom, not a forecast. The cost
/// is on each of the 17 ports the capacity and its note-off room at 32 bytes
/// an event: 0.3 MB in all at 512 events, 1.3 MB at 2,048 (a 16,384-frame
/// block); and for each port a PDC delay is on, 72 bytes an entry of `3.75 ×`
/// that (0.16 MB at 512, 0.62 MB at 2,048).
pub const MIDI_INPUT_DENSITY: EventDensity = EventDensity::per_kiloframe(128);

/// The port an event leaves a [`MidiInputNode`] from.
pub fn input_port_of(event: &MidiEvent) -> usize {
    event
        .channel()
        .map_or(CHANNELLESS_PORT, |c| usize::from(c.get()))
}

/// The ports a node listening to `channel` (every channel when `None`) takes
/// edges from: that channel's and the channelless port, or all of them.
pub fn input_ports(channel: Option<MidiChannel>) -> impl Iterator<Item = usize> {
    let (a, b) = match channel {
        Some(c) => (usize::from(c.get()), usize::from(c.get()) + 1),
        None => (0, CHANNELLESS_PORT),
    };
    (a..b).chain(std::iter::once(CHANNELLESS_PORT))
}

/// A MIDI input edge as a graph node with [`MIDI_INPUT_PORTS`] event outputs.
/// See the module docs.
///
/// ```
/// use tutti_types::{NodeKey, SampleRate, Samples};
/// use tutti_graph::{Editor, Prepare};
/// use tutti_midi_runtime::MidiInputNode;
///
/// let (mut editor, _exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
/// // No wire: a node the host fills later, or a test's.
/// let controls = editor.insert(NodeKey(1), "midi in", MidiInputNode::new(None));
/// assert_eq!(controls.pending_mpe_mode(), None);
/// ```
pub struct MidiInputNode {
    /// The edge this node polls, owned: whatever state a poll keeps (a
    /// hardware drain's scratch, its read of the port set) is the node's.
    input: Option<Box<dyn MidiIn>>,
    /// What the wire's poll fills.
    scratch: Box<[MidiEvent]>,
    /// What the block sends: the translated events, which may outnumber the
    /// polled ones (a member channel's bend range fans out over its zone).
    out: Box<[MidiEvent]>,
    translator: Midi1ToMidi2Translator,
    mpe: MpeIngest,
    /// The MPE mode a host asked for, read once per block. The reader owns
    /// its cell (`Reader::cell`), which the controls publish to.
    mpe_request: Reader<Option<MpeMode>>,
    /// Translated events a block had no room for in `out`
    /// ([`MidiInputControls::dropped_events`]).
    dropped: Arc<AtomicU64>,
    /// The events a block polls and each port holds, from the block the node
    /// was prepared for.
    capacity: usize,
}

impl std::fmt::Debug for MidiInputNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiInputNode")
            .field("translator", &self.translator)
            .field("mpe", &self.mpe)
            .field("mpe_request", &self.mpe_request)
            .field("dropped", &self.dropped)
            .finish_non_exhaustive()
    }
}

impl MidiInputNode {
    /// A node polling `input` each block (`None`: a node that sends nothing),
    /// with MPE ingestion off. The node owns `input`: build one per node (for
    /// the hardware ports, `tutti_midi_hardware::HardwareMidiDrain::new`
    /// over the shared `Arc<HardwareMidiInputs>`).
    #[must_use]
    pub fn new(input: Option<Box<dyn MidiIn>>) -> Self {
        let cell = Arc::new(ArcLend::new(None));
        Self {
            mpe_request: cell.reader(),
            input,
            scratch: vec![MidiEvent::noop(); MIDI_INPUT_EVENT_CAPACITY].into_boxed_slice(),
            out: vec![MidiEvent::noop(); tutti_graph::note_off_room(MIDI_INPUT_EVENT_CAPACITY)]
                .into_boxed_slice(),
            translator: Midi1ToMidi2Translator::new(),
            mpe: MpeIngest::new(MpeMode::Disabled),
            dropped: Arc::new(AtomicU64::new(0)),
            capacity: MIDI_INPUT_EVENT_CAPACITY,
        }
    }

    /// Ingest classic MPE in `mode` from the first block.
    #[must_use]
    pub fn with_mpe(mut self, mode: MpeMode) -> Self {
        self.mpe = MpeIngest::new(mode);
        self
    }

    /// Adopt a requested MPE mode that differs from the one in force, and
    /// return the new zones' default bend ranges for the block to send first
    /// ([`MpeIngest::zone_defaults`]: what an MPE Configuration Message
    /// resets a receiver to, as this node sends an MCM's).
    ///
    /// **The comparison is load-bearing**: the request latches (the audio
    /// thread cannot clear it without publishing), and `set_mode` rebuilds
    /// the ingest, so re-adopting it every block would drop every sounding
    /// note's voice mapping, and re-send the ranges, every block.
    fn adopt_mpe_request(&mut self) -> Option<Translated> {
        let mode = (*self.mpe_request.lend())?;
        if *self.mpe.mode() == mode {
            return None;
        }
        self.mpe.set_mode(mode);
        Some(self.mpe.zone_defaults(MidiGroup::FIRST, 0))
    }
}

/// The host's handle on a [`MidiInputNode`]: its MPE mode. Cheap to clone.
#[derive(Clone, Debug)]
pub struct MidiInputControls {
    /// What the node reads; the controls publish to it and read it back with
    /// `ArcLend::current`.
    request: Arc<ArcLend<Option<MpeMode>>>,
    dropped: Arc<AtomicU64>,
}

impl MidiInputControls {
    /// Ingest classic MPE in `mode` from the next block. Control thread.
    ///
    /// A new mode resets MPE voice allocation: a note sounding through a
    /// member channel loses its mapping. Setting the mode in force changes
    /// nothing.
    pub fn set_mpe_mode(&self, mode: MpeMode) {
        self.request.publish(Some(mode));
    }

    /// Frees the requests the node has let go of since the last
    /// [`set_mpe_mode`](Self::set_mpe_mode), without publishing; returns how
    /// many are still held. Control thread, from the owner's tick (a
    /// session's `Reclaim` phase sweeps its hardware input's); with nothing
    /// retired it is a lock and a length check.
    pub fn reclaim(&self) -> usize {
        self.request.reclaim()
    }

    /// How many translated events the node has dropped since it was built:
    /// past the node's capacity in one block, an event's
    /// translation (a bend range or an MPE reset fans out over a zone) is
    /// dropped whole rather than cut partway. A poll takes no more than the
    /// capacity: a hardware wire leaves the rest in its rings for the next
    /// block, and counts what its rings had no room for.
    pub fn dropped_events(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// The mode last asked for through [`set_mpe_mode`](Self::set_mpe_mode),
    /// if any: what was asked, not what is in force (they differ for a block
    /// after a change, and for good if the graph never runs). Control side:
    /// the cell's own value ([`ArcLend::current`]), never a lend.
    pub fn pending_mpe_mode(&self) -> Option<MpeMode> {
        *self.request.current()
    }
}

impl Node for MidiInputNode {
    /// No audio; [`MIDI_INPUT_PORTS`] event outputs of the prepared
    /// capacity ([`MIDI_INPUT_EVENT_CAPACITY`] at least, and
    /// [`MIDI_INPUT_DENSITY`] per frame of a long block).
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
            .with_events(0, MIDI_INPUT_PORTS as u16)
            .with_event_capacity(self.capacity as u32)
            // Event-only: a poll and a translation per event, nothing per
            // frame.
            .with_cost(tutti_graph::Cost::Light)
    }

    fn prepare(&mut self, p: &Prepare) {
        let capacity = (MIDI_INPUT_DENSITY.capacity(p) as usize).max(MIDI_INPUT_EVENT_CAPACITY);
        if capacity != self.capacity || self.scratch.len() != capacity {
            self.capacity = capacity;
            self.scratch = vec![MidiEvent::noop(); capacity].into_boxed_slice();
            self.out =
                vec![MidiEvent::noop(); tutti_graph::note_off_room(capacity)].into_boxed_slice();
        }
        if let Some(input) = self.input.as_mut() {
            input.reserve(capacity);
        }
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        // Before any event is translated, stated or not.
        let reset = self.adopt_mpe_request();
        let Some(input) = self.input.as_mut() else {
            return Status::Modified;
        };
        let block_len = cx.env.block_len;
        let mut kept = 0;
        let mut dropped = 0;
        // An event's translation goes in whole or not at all: a fan-out cut
        // partway would leave some of a zone's channels at another range.
        // A note-off may use the room past the capacity (the port's own
        // `note_off_room`), so a block full of other messages (a bend range
        // fanned out over a zone) cannot keep it out.
        let capacity = self.capacity;
        let mut stage = |events: Translated, note_off: bool| {
            let n = events.len();
            let limit = if note_off {
                self.out.len()
            } else {
                capacity.min(self.out.len())
            };
            match (kept + n <= limit).then(|| &mut self.out[kept..kept + n]) {
                Some(slots) => {
                    for (slot, ev) in slots.iter_mut().zip(events) {
                        *slot = ev;
                    }
                    kept += n;
                }
                None => dropped += n as u64,
            }
        };
        // A host-set mode's ranges, at frame 0, ahead of the block's events
        // (the sort below is stable).
        if let Some(reset) = reset {
            stage(reset, false);
        }
        let n = input.poll_block(block_len.get(), &mut self.scratch);
        for raw in &self.scratch[..n.min(self.scratch.len())] {
            let Some(ev) = self.translator.translate(raw) else {
                continue;
            };
            stage(self.mpe.translate(&ev), ev.is_note_off());
        }
        if dropped > 0 {
            self.dropped.fetch_add(dropped, Ordering::Relaxed);
            // The executor's too, so an export watching its count sees it.
            io.count_lost(dropped);
        }
        let events = &mut self.out[..kept];
        in_offset_order(events);
        for ev in events.iter() {
            // Refused past the capacity: counted by the executor.
            let _ = io
                .event_out(input_port_of(ev))
                .push(Event::midi(offset_of(ev, block_len), ev.data));
        }
        Status::Modified
    }

    fn reset(&mut self) {}
}

/// A [`MidiInputNode`]'s fork: the same ports, no wire.
struct SilentInput;

impl ForkSource for SilentInput {
    fn fork(&self, _mode: ForkMode) -> Result<Forked, ForkCause> {
        Ok(Forked::new(Box::new(MidiInputNode::new(None))))
    }
}

impl IntoNode for MidiInputNode {
    type Controls = MidiInputControls;

    fn into_parts(self) -> NodeParts<MidiInputControls> {
        let controls = MidiInputControls {
            request: Arc::clone(self.mpe_request.cell()),
            dropped: Arc::clone(&self.dropped),
        };
        NodeParts {
            node: Box::new(self),
            controls,
            fork: Some(Box::new(SilentInput)),
        }
    }
}

#[cfg(test)]
mod tests;
