//! [`MidiInputNode`]: a MIDI input edge (the hardware ports, typically) as a
//! graph node, its events out of one event port per Group and channel.
//!
//! Each block it polls its [`MidiIn`], translates MIDI 1.0 to MIDI 2.0
//! ([`Translator`]: RPN and NRPN runs, Bank Select, widening) and rewrites
//! classic MPE into native per-note messages ([`MpeIngest`]), then sends every
//! event out of the port for its address ([`MidiInputPorts`]). A Group is a
//! channel space of its own (M2-104 section 2.1.2, Appendix B.1: a message in
//! one Group does not apply to Channels in another), so a Group has its own
//! sixteen channel ports and its own channelless port, which carries that
//! Group's System, SysEx and Flex messages. The node hears the first Group
//! unless built to hear more ([`MidiInputNode::hearing`]), so a one-Group
//! device is the seventeen ports it always was. A message with no Group
//! (Utility, UMP Stream: they address the Endpoint, not a Group) leaves the
//! first Group's channelless port. **Routing is wiring**: a synth listening
//! to channel 3 of a Group takes event edges from that Group's port 3 and its
//! channelless port, one listening to a whole Group from its seventeen
//! ([`MidiInputPorts::listening`]).
//!
//! Translation runs on every event, wired or not: both stages are stateful
//! assemblers, and skipping an event corrupts the ones after it.
//!
//! Its fork is silent: an offline render reads no wire.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tutti_graph::UmpAt;

use tutti_graph::{
    Cx, Event, EventDensity, ForkCause, ForkMode, ForkSource, Forked, IntoNode, Io, Node,
    NodeParts, Offset, Prepare, Shape, Status,
};
use tutti_midi_protocol::MpeMode;
use tutti_midi_protocol::Translator;
use tutti_rt::{ArcLend, Reader};
use tutti_ump::{MidiChannel, MidiGroup, Ump};

use super::order::{BLANK, in_offset_order, offset_of};
use crate::outbound::mpe_ingest::MpeIngest;
use crate::traits::MidiIn;

/// The port that carries a Group's events with no channel: after its sixteen
/// channel ports. For the first Group (and a node that hears only it); Group
/// `g`'s is [`MidiInputPorts::channelless`].
pub const CHANNELLESS_PORT: usize = MidiChannel::COUNT as usize;

/// How many event outputs a [`MidiInputNode`] that hears one Group has: one
/// per channel and [`CHANNELLESS_PORT`]. Each further Group adds as many
/// ([`MidiInputPorts::count`]).
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

/// Which output port of a [`MidiInputNode`] carries what: the layout of the
/// Groups it hears.
///
/// Group `g` owns the ports `g * 17 ..= g * 17 + 16`: channel `c`'s is
/// `g * 17 + c`, and the seventeenth ([`channelless`](Self::channelless)) is
/// the Group's messages that have no channel. The Groups a node hears are the
/// first ones, `0` through the last it was built for, so the first Group's
/// ports are the same whatever follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MidiInputPorts {
    /// How many Groups, from the first: 1 to 16.
    groups: u8,
}

impl MidiInputPorts {
    /// The ports of a node that hears the first Group alone: the default, and
    /// [`MIDI_INPUT_PORTS`] of them.
    pub const FIRST_GROUP: Self = Self::through(MidiGroup::FIRST);

    /// The ports of a node that hears every Group up to and including `last`.
    #[must_use]
    pub const fn through(last: MidiGroup) -> Self {
        Self {
            groups: last.get() + 1,
        }
    }

    /// How many event outputs the node has.
    #[must_use]
    pub const fn count(self) -> usize {
        self.groups as usize * PORTS_PER_GROUP
    }

    /// The last Group the node hears.
    #[must_use]
    pub const fn last_group(self) -> MidiGroup {
        MidiGroup::masked(self.groups - 1)
    }

    /// The port of `group`'s events with no channel (System, SysEx, Flex).
    /// `None` for a Group the node does not hear.
    #[must_use]
    pub const fn channelless(self, group: MidiGroup) -> Option<usize> {
        if group.get() < self.groups {
            Some(group.get() as usize * PORTS_PER_GROUP + CHANNELLESS_PORT)
        } else {
            None
        }
    }

    /// The port channel `channel` of `group` leaves from. `None` for a Group
    /// the node does not hear.
    #[must_use]
    pub const fn channel(self, group: MidiGroup, channel: MidiChannel) -> Option<usize> {
        if group.get() < self.groups {
            Some(group.get() as usize * PORTS_PER_GROUP + channel.get() as usize)
        } else {
            None
        }
    }

    /// The port an event leaves from: its channel's in its Group, a Group's
    /// channelless port for a message that has a Group but no channel, and the
    /// first Group's channelless port for one with no Group. `None` for an
    /// event of a Group the node does not hear: it was addressed to a
    /// Group nothing here listens to, so it is not delivered.
    #[must_use]
    pub fn port_of(self, event: &Ump) -> Option<usize> {
        match (event.address(), event.group()) {
            (Some(a), _) => self.channel(a.group, a.channel),
            (None, Some(group)) => self.channelless(group),
            (None, None) => self.channelless(MidiGroup::FIRST),
        }
    }

    /// The ports a listener takes edges from: the ports of `channel` in
    /// `group`, and that Group's channelless port. `None` for `channel` is
    /// every channel of the Group, and `None` for `group` is every Group the
    /// node hears (the first Group's channelless port then also carries
    /// what has no Group). A Group the node does not hear has no ports.
    pub fn listening(
        self,
        group: Option<MidiGroup>,
        channel: Option<MidiChannel>,
    ) -> impl Iterator<Item = usize> {
        let (from, to) = match group {
            Some(g) => (g.get(), g.get() + 1),
            None => (0, self.groups),
        };
        (from..to.min(self.groups)).flat_map(move |g| {
            let base = usize::from(g) * PORTS_PER_GROUP;
            let (a, b) = match channel {
                Some(c) => (usize::from(c.get()), usize::from(c.get()) + 1),
                None => (0, CHANNELLESS_PORT),
            };
            (base + a..base + b).chain(std::iter::once(base + CHANNELLESS_PORT))
        })
    }
}

impl Default for MidiInputPorts {
    fn default() -> Self {
        Self::FIRST_GROUP
    }
}

/// A Group's ports: its sixteen channels and its channelless port.
const PORTS_PER_GROUP: usize = MIDI_INPUT_PORTS;

/// A MIDI input edge as a graph node with [`MidiInputPorts::count`] event
/// outputs ([`MIDI_INPUT_PORTS`] for the first Group alone).
/// See the module docs.
///
/// ```
/// use tutti_types::{SampleRate, Samples};
/// use tutti_graph::NodeKey;
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
    scratch: Box<[UmpAt]>,
    /// What the block sends: the translated events, which may outnumber the
    /// polled ones (a member channel's bend range fans out over its zone).
    out: Box<[UmpAt]>,
    /// The Groups it hears, and so its ports.
    ports: MidiInputPorts,
    translator: Translator,
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
            scratch: vec![BLANK; MIDI_INPUT_EVENT_CAPACITY].into_boxed_slice(),
            out: vec![BLANK; tutti_graph::note_off_room(MIDI_INPUT_EVENT_CAPACITY)]
                .into_boxed_slice(),
            ports: MidiInputPorts::FIRST_GROUP,
            translator: Translator::new(),
            mpe: MpeIngest::new(MpeMode::Disabled),
            dropped: Arc::new(AtomicU64::new(0)),
            capacity: MIDI_INPUT_EVENT_CAPACITY,
        }
    }

    /// Hear the Groups of `ports` (the first Group alone by default): each
    /// has its own channel ports and channelless port, and an event of a Group
    /// past them is not delivered ([`MidiInputPorts::port_of`]). The node's
    /// event outputs are [`MidiInputPorts::count`].
    #[must_use]
    pub fn hearing(mut self, ports: MidiInputPorts) -> Self {
        self.ports = ports;
        self
    }

    /// Ingest classic MPE in `mode` from the first block, on every Group.
    #[must_use]
    pub fn with_mpe(mut self, mode: MpeMode) -> Self {
        self.mpe = MpeIngest::new(mode);
        self
    }

    /// Adopt a requested MPE mode that differs from the one in force; `true`
    /// when it did, and the block then sends the new zones' default bend
    /// ranges first ([`MpeIngest::zone_defaults`]: what an MPE Configuration
    /// Message resets a receiver to, as this node sends an MCM's).
    ///
    /// **The comparison is load-bearing**: the request latches (the audio
    /// thread cannot clear it without publishing), and `set_mode` rebuilds
    /// the ingest, so re-adopting it every block would drop every sounding
    /// note's voice mapping, and re-send the ranges, every block.
    fn adopt_mpe_request(&mut self) -> bool {
        let Some(mode) = *self.mpe_request.lend() else {
            return false;
        };
        if *self.mpe.mode() == mode {
            return false;
        }
        self.mpe.set_mode(mode);
        true
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
    /// No audio; [`MidiInputPorts::count`] event outputs ([`MIDI_INPUT_PORTS`]
    /// for the first Group alone) of the prepared capacity ([`MIDI_INPUT_EVENT_CAPACITY`] at least, and
    /// [`MIDI_INPUT_DENSITY`] per frame of a long block).
    fn shape(&self) -> Shape {
        Shape::EMPTY
            .with_events(0, self.ports.count() as u16)
            .with_event_capacity(self.capacity as u32)
            // Event-only: a poll and a translation per event, nothing per
            // frame.
            .with_cost(tutti_graph::Cost::Light)
    }

    fn prepare(&mut self, p: &Prepare) {
        let capacity = (MIDI_INPUT_DENSITY.capacity(p) as usize).max(MIDI_INPUT_EVENT_CAPACITY);
        if capacity != self.capacity || self.scratch.len() != capacity {
            self.capacity = capacity;
            self.scratch = vec![BLANK; capacity].into_boxed_slice();
            self.out = vec![BLANK; tutti_graph::note_off_room(capacity)].into_boxed_slice();
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
        // An event's translation goes in whole or not at all: a fan-out cut
        // partway would leave some of a zone's channels at another range.
        // A note-off may use the room past the capacity (the port's own
        // `note_off_room`), so a block full of other messages (a bend range
        // fanned out over a zone) cannot keep it out.
        let mut staging = Staging {
            out: &mut self.out,
            capacity: self.capacity,
            kept: 0,
            dropped: 0,
        };
        // A host-set mode's ranges, at frame 0, ahead of the block's events
        // (the sort below is stable).
        if reset {
            for group in 0..self.ports.last_group().get() + 1 {
                staging.whole(false, |send| {
                    self.mpe
                        .zone_defaults(MidiGroup::masked(group), Offset::ZERO, send);
                });
            }
        }
        let n = input.poll_block(block_len, &mut self.scratch);
        for &(at, raw) in &self.scratch[..n.min(self.scratch.len())] {
            let (translator, mpe) = (&mut self.translator, &mut self.mpe);
            translator.feed(raw, |ev| {
                staging.whole(ev.is_note_off(), |send| mpe.translate(&(at, ev), send));
            });
        }
        let Staging { kept, dropped, .. } = staging;
        if dropped > 0 {
            self.dropped.fetch_add(dropped, Ordering::Relaxed);
            // The executor's too, so an export watching its count sees it.
            io.count_lost(dropped);
        }
        let events = &mut self.out[..kept];
        in_offset_order(events);
        for ev in events.iter() {
            // A Group the node does not hear has no port.
            let Some(port) = self.ports.port_of(&ev.1) else {
                continue;
            };
            // Refused past the capacity: counted by the executor.
            let _ = io
                .event_out(port)
                .push(Event::midi(offset_of(ev, block_len), ev.1));
        }
        Status::Modified
    }

    fn reset(&mut self) {}
}

/// The block's translated events, as they are made.
struct Staging<'a> {
    out: &'a mut [UmpAt],
    capacity: usize,
    kept: usize,
    dropped: u64,
}

impl Staging<'_> {
    /// Keeps everything `make` sends, or, if it does not all fit, none of it
    /// and counts it dropped. A `note_off`'s may use the room past the
    /// capacity.
    fn whole(&mut self, note_off: bool, make: impl FnOnce(&mut dyn FnMut(UmpAt))) {
        let limit = if note_off {
            self.out.len()
        } else {
            self.capacity.min(self.out.len())
        };
        let start = self.kept;
        let mut sent = 0u64;
        let mut fits = true;
        make(&mut |ev| {
            sent += 1;
            if fits && self.kept < limit {
                self.out[self.kept] = ev;
                self.kept += 1;
            } else {
                fits = false;
            }
        });
        if !fits {
            self.kept = start;
            self.dropped += sent;
        }
    }
}

/// A [`MidiInputNode`]'s fork: the same ports, no wire.
struct SilentInput(MidiInputPorts);

impl ForkSource for SilentInput {
    fn fork(&self, _mode: ForkMode) -> Result<Forked, ForkCause> {
        Ok(Forked::new(Box::new(
            MidiInputNode::new(None).hearing(self.0),
        )))
    }
}

impl IntoNode for MidiInputNode {
    type Controls = MidiInputControls;

    fn into_parts(self) -> NodeParts<MidiInputControls> {
        let ports = self.ports;
        let controls = MidiInputControls {
            request: Arc::clone(self.mpe_request.cell()),
            dropped: Arc::clone(&self.dropped),
        };
        NodeParts {
            node: Box::new(self),
            controls,
            fork: Some(Box::new(SilentInput(ports))),
        }
    }
}

#[cfg(test)]
mod tests;
