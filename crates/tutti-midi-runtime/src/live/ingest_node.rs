//! [`MidiIngestNode`]: the graph side of the MIDI input edge, its events out of
//! one event port per Group and channel.
//!
//! A hardware device reaches the graph through an event inlet of its own
//! ([`DeviceInlets`](crate::DeviceInlets)), each event already on its frame
//! (the device's thread stamps it, `tutti-graph`'s event inlet holds it until
//! its block); the node has one event input per device slot, so what it reads
//! is what the inlets emitted. Each block it translates every slot's events,
//! MIDI 1.0 to MIDI 2.0 ([`Translator`]: RPN and NRPN runs, Bank Select,
//! widening), and rewrites classic MPE into native per-note messages
//! ([`MpeIngest`]), then sends every event out of the port for its address
//! ([`MidiInputPorts`]). **The translators and the MPE state are per slot**:
//! the runs a device's controller sends are its own, and one device's Data
//! Entry never completes another's RPN. A Group is a channel space of its own
//! (M2-104 section 2.1.2, Appendix B.1: a message in one Group does not apply
//! to Channels in another), so a Group has its own sixteen channel ports and
//! its own channelless port, which carries that Group's System, SysEx and Flex
//! messages. The node hears the first Group unless built to hear more
//! ([`MidiIngestNode::hearing`]), so a one-Group device is the seventeen ports
//! it always was. A message with no Group (Utility, UMP Stream: they address
//! the Endpoint, not a Group) leaves the first Group's channelless port.
//! **Routing is wiring**: a synth listening to channel 3 of a Group takes event
//! edges from that Group's port 3 and its channelless port, one listening to a
//! whole Group from its seventeen ([`MidiInputPorts::listening`]); the devices
//! merge in the listener's event fan-in.
//!
//! Translation runs on every event, wired or not: both stages are stateful
//! assemblers, and skipping an event corrupts the ones after it.
//!
//! A slot whose device changed is restarted
//! ([`MidiIngestControls::restart_slot`]): its translator and its MPE state
//! start empty, so one device's half-sent run is not completed by the next.
//!
//! Its fork is silent: an offline render reads no wire.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use tutti_graph::UmpAt;

use tutti_graph::{
    Cx, Event, EventDensity, EventKind, ForkCause, ForkMode, ForkSource, Forked, IntoNode, Io,
    Node, NodeParts, Offset, Prepare, Shape, Status,
};
use tutti_midi_protocol::MpeMode;
use tutti_midi_protocol::Translator;
use tutti_rt::{ArcLend, Reader};
use tutti_ump::{MidiChannel, MidiGroup, Ump};

use super::order::{BLANK, in_offset_order, offset_of};
use crate::outbound::mpe_ingest::MpeIngest;

/// The port that carries a Group's events with no channel: after its sixteen
/// channel ports. For the first Group (and a node that hears only it); Group
/// `g`'s is [`MidiInputPorts::channelless`].
pub const CHANNELLESS_PORT: usize = MidiChannel::COUNT as usize;

/// How many event outputs a [`MidiIngestNode`] that hears one Group has: one
/// per channel and [`CHANNELLESS_PORT`]. Each further Group adds as many
/// ([`MidiInputPorts::count`]).
pub const MIDI_INPUT_PORTS: usize = CHANNELLESS_PORT + 1;

/// The fewest events a [`MidiIngestNode`] translates in one block, and each port's
/// declared event capacity at the shortest blocks.
pub const MIDI_INPUT_EVENT_CAPACITY: usize = 512;

/// The events per frame a [`MidiIngestNode`] is sized for: one in eight
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

/// Which output port of a [`MidiIngestNode`] carries what: the layout of the
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

/// The MIDI input edge as a graph node: one event input per device slot and
/// [`MidiInputPorts::count`] event outputs ([`MIDI_INPUT_PORTS`] for the first
/// Group alone). See the module docs.
///
/// ```
/// use tutti_types::MaxBlock;
/// use tutti_types::{SampleRate, Samples};
/// use tutti_graph::NodeKey;
/// use tutti_graph::{Editor, Prepare};
/// use tutti_midi_runtime::MidiIngestNode;
///
/// let (mut editor, _exec) = Editor::new(Prepare::new(SampleRate(48_000.0), MaxBlock::of(Samples(512))));
/// // Four device slots, none wired yet.
/// let controls = editor.insert(NodeKey(1), "midi in", MidiIngestNode::new(4));
/// assert_eq!(controls.pending_mpe_mode(), None);
/// ```
pub struct MidiIngestNode {
    /// One translation state per device slot.
    devices: Box<[Device]>,
    /// How many times each slot was restarted
    /// ([`MidiIngestControls::restart_slot`]), as the node last saw it.
    restarts: Arc<[AtomicU32]>,
    /// What the block sends: the translated events, which may outnumber the
    /// ones read (a member channel's bend range fans out over its zone).
    out: Box<[UmpAt]>,
    /// The Groups it hears, and so its ports.
    ports: MidiInputPorts,
    /// The MPE mode in force on every slot.
    mpe_mode: MpeMode,
    /// The MPE mode a host asked for, read once per block. The reader owns
    /// its cell (`Reader::cell`), which the controls publish to.
    mpe_request: Reader<Option<MpeMode>>,
    /// Translated events a block had no room for in `out`
    /// ([`MidiIngestControls::dropped_events`]).
    dropped: Arc<AtomicU64>,
    /// The events a block translates and each port holds, from the block the
    /// node was prepared for.
    capacity: usize,
}

/// What one device slot keeps between blocks: its MIDI 1.0 assembler and its
/// classic-MPE state.
struct Device {
    translator: Translator,
    mpe: MpeIngest,
    /// The slot's restart count this state was last reset at.
    restarted: u32,
}

impl Device {
    fn new(mode: MpeMode) -> Self {
        Self {
            translator: Translator::new(),
            mpe: MpeIngest::new(mode),
            restarted: 0,
        }
    }

    /// Empty, as for a device that has not sent anything.
    fn restart(&mut self, mode: MpeMode, restarted: u32) {
        self.translator.reset();
        self.mpe = MpeIngest::new(mode);
        self.restarted = restarted;
    }
}

impl std::fmt::Debug for MidiIngestNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiIngestNode")
            .field("devices", &self.devices.len())
            .field("mpe_mode", &self.mpe_mode)
            .field("mpe_request", &self.mpe_request)
            .field("dropped", &self.dropped)
            .finish_non_exhaustive()
    }
}

impl MidiIngestNode {
    /// A node with `devices` event inputs, one per device slot (at least one),
    /// with MPE ingestion off.
    #[must_use]
    pub fn new(devices: usize) -> Self {
        let devices = devices.max(1);
        let cell = Arc::new(ArcLend::new(None));
        Self {
            mpe_request: cell.reader(),
            devices: (0..devices)
                .map(|_| Device::new(MpeMode::Disabled))
                .collect(),
            restarts: (0..devices).map(|_| AtomicU32::new(0)).collect(),
            out: vec![BLANK; tutti_graph::note_off_room(MIDI_INPUT_EVENT_CAPACITY)]
                .into_boxed_slice(),
            ports: MidiInputPorts::FIRST_GROUP,
            mpe_mode: MpeMode::Disabled,
            dropped: Arc::new(AtomicU64::new(0)),
            capacity: MIDI_INPUT_EVENT_CAPACITY,
        }
    }

    /// How many device slots, which is how many event inputs.
    #[must_use]
    pub fn devices(&self) -> usize {
        self.devices.len()
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
        self.mpe_mode = mode;
        for device in &mut self.devices {
            device.mpe = MpeIngest::new(mode);
        }
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
        if self.mpe_mode == mode {
            return false;
        }
        self.mpe_mode = mode;
        for device in &mut self.devices {
            device.mpe.set_mode(mode);
        }
        true
    }
}

/// The host's handle on a [`MidiIngestNode`]: its MPE mode, and which device
/// slots start over. Cheap to clone.
#[derive(Clone, Debug)]
pub struct MidiIngestControls {
    /// What the node reads; the controls publish to it and read it back with
    /// `ArcLend::current`.
    request: Arc<ArcLend<Option<MpeMode>>>,
    dropped: Arc<AtomicU64>,
    restarts: Arc<[AtomicU32]>,
}

impl MidiIngestControls {
    /// Ingest classic MPE in `mode` from the next block. Control thread.
    ///
    /// A new mode stops and resets the channels that enter or leave MPE
    /// control (M1-100 §2.2.3: All Sound Off and Reset All Controllers, sent
    /// ahead of the new zones' default bend ranges); the others keep their
    /// notes. Setting the mode in force changes nothing.
    pub fn set_mpe_mode(&self, mode: MpeMode) {
        self.request.publish(Some(mode));
    }

    /// Starts device slot `slot` over from the next block: its translator and
    /// its MPE state are emptied, as for a device that has sent nothing. What
    /// a new device taking the slot needs, so the previous one's half-sent
    /// RPN or held MPE notes are not carried into it. `false` for a slot the
    /// node has no input for. Control thread; lock-free.
    pub fn restart_slot(&self, slot: usize) -> bool {
        match self.restarts.get(slot) {
            Some(count) => {
                count.fetch_add(1, Ordering::Relaxed);
                true
            }
            None => false,
        }
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
    /// dropped whole rather than cut partway. What a device's inlet could not
    /// hold is counted at the inlet, not here
    /// ([`DeviceInlets`](crate::DeviceInlets)).
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

impl Node for MidiIngestNode {
    /// No audio; one event input per device slot, [`MidiInputPorts::count`]
    /// event outputs ([`MIDI_INPUT_PORTS`] for the first Group alone) of the
    /// prepared capacity ([`MIDI_INPUT_EVENT_CAPACITY`] at least, and
    /// [`MIDI_INPUT_DENSITY`] per frame of a long block).
    fn shape(&self) -> Shape {
        Shape::EMPTY
            .with_events(self.devices.len() as u16, self.ports.count() as u16)
            .with_event_capacity(self.capacity as u32)
            // Event-only: a translation per event, nothing per frame.
            .with_cost(tutti_graph::Cost::Light)
    }

    fn prepare(&mut self, p: &Prepare) {
        let capacity = (MIDI_INPUT_DENSITY.capacity(p) as usize).max(MIDI_INPUT_EVENT_CAPACITY);
        if capacity != self.capacity {
            self.capacity = capacity;
            self.out = vec![BLANK; tutti_graph::note_off_room(capacity)].into_boxed_slice();
        }
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        // Before any event is translated, stated or not.
        let reset = self.adopt_mpe_request();
        let mode = self.mpe_mode;
        for (device, restarts) in self.devices.iter_mut().zip(self.restarts.iter()) {
            let restarts = restarts.load(Ordering::Relaxed);
            if restarts != device.restarted {
                device.restart(mode, restarts);
            }
        }
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
        // (the sort below is stable). Once, whatever the devices: they are
        // the receiver's, not a device's.
        if reset {
            for group in 0..self.ports.last_group().get() + 1 {
                staging.whole(false, |send| {
                    self.devices[0]
                        .mpe
                        .zone_defaults(MidiGroup::masked(group), Offset::ZERO, send);
                });
            }
        }
        for (slot, device) in self.devices.iter_mut().enumerate() {
            let (translator, mpe) = (&mut device.translator, &mut device.mpe);
            for event in io.events(slot) {
                let EventKind::Midi(raw) = event.kind else {
                    continue;
                };
                let at = event.offset;
                translator.feed(raw, |ev| {
                    staging.whole(ev.is_note_off(), |send| mpe.translate(&(at, ev), send));
                });
            }
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

/// A [`MidiIngestNode`]'s fork: the same slots and ports, nothing to read.
struct SilentInput {
    ports: MidiInputPorts,
    devices: usize,
}

impl ForkSource for SilentInput {
    fn fork(&self, _mode: ForkMode) -> Result<Forked, ForkCause> {
        Ok(Forked::new(Box::new(
            MidiIngestNode::new(self.devices).hearing(self.ports),
        )))
    }
}

impl IntoNode for MidiIngestNode {
    type Controls = MidiIngestControls;

    fn into_parts(self) -> NodeParts<MidiIngestControls> {
        let fork = SilentInput {
            ports: self.ports,
            devices: self.devices.len(),
        };
        let controls = MidiIngestControls {
            request: Arc::clone(self.mpe_request.cell()),
            dropped: Arc::clone(&self.dropped),
            restarts: Arc::clone(&self.restarts),
        };
        NodeParts {
            node: Box::new(self),
            controls,
            fork: Some(Box::new(fork)),
        }
    }
}

#[cfg(test)]
mod tests;
