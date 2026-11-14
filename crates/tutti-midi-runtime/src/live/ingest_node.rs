//! [`MidiIngestNode`]: the graph side of the MIDI input edge, its events out of
//! one event port per Group and channel.
//!
//! A hardware device reaches the graph through an event inlet of its own
//! ([`DeviceInlets`]), each event already on its frame
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
//! **A source that ends leaves nothing held downstream.** Each slot keeps what
//! its device started and has not ended ([`HeldState`]: the keys down, the hold
//! pedals down and the bends off centre, as the device sent them), and
//! releases it as the messages that end it (Note Offs, then pedal-ups and
//! centred bends; `HeldState`'s docs say why not All Notes Off) through the
//! same translation as any other message, in three cases:
//!
//! - *The device changed or went.* Built [`following`](MidiIngestNode::following)
//!   the [`DeviceInlets`] its inputs are fed from, the node compares each
//!   slot's epoch (which the inlets bump as each device takes the slot and as
//!   it lets go) with the one it last saw every block. A slot that differs
//!   releases everything the old device held, at the block's first frame, then
//!   starts empty, translator and MPE state, so one device's half-sent run is
//!   not completed by the next; and a slot whose device went hears nothing
//!   until another takes it. (The executor's edge close, `tutti-graph`'s
//!   `held.rs`, ends the same notes when the device's inlet is taken out of the
//!   graph; this ends them as soon as the device lets go, and the pedals and
//!   bends the edge does not track.)
//! - *Active Sensing lapsed* ([`ActiveSensing`], MIDI 1.0 Detailed
//!   Specification p. 32). Once a Group has sent an Active Sensing (FE), a
//!   message is expected from it at least every 330 ms; when none comes, the
//!   Group's held notes and controllers are released at the frame the time ran
//!   out. The watch counts the graph's frames, which are the arrival clock the
//!   inlets placed each event on, so the node reads no OS clock.
//! - *System Reset* (FF, p. 33). The Group's held state is released, its
//!   translator state and MPE state are reset, and its watch is dropped: voices
//!   off, controllers reset, running status cleared, back to the power-up
//!   condition.
//!
//! The three share one rule: a release the block has no room for stays held
//! and is owed, and goes out at the next block's first frame. A lapse releases
//! the Group's held state and leaves the translator and MPE state alone (the
//! specification asks only that voices go quiet); an engine stall does not
//! lapse a cable: what arrived during it is placed late, at the first frame of
//! the block that finds it, and clears the timer there. No input path keeps a byte parser (ALSA and
//! CoreMIDI deliver UMP), so no running status or SysEx reassembly is stranded
//! at this edge; Set Local On, Omni On or Poly, Song Position 0 and Stop belong
//! to the transport and the synth, and nothing here holds them.
//!
//! The FE and the FF are the cable's own: the node acts on them and sends
//! neither on (an FF is never echoed, p. 33).
//!
//! Its fork is silent: an offline render reads no wire.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tutti_graph::{Losses, UmpAt};

use tutti_graph::{
    Cx, Event, EventDensity, EventKind, ForkCause, ForkMode, ForkSource, Forked, IntoNode, Io,
    Node, NodeParts, Offset, Prepare, Shape, Status,
};
use tutti_midi_protocol::Translator;
use tutti_midi_protocol::{MpeMode, MpeZones};
use tutti_rt::{ArcLend, Reader};
use tutti_types::{Frame, SampleRate, Samples};
use tutti_ump::{Message, MidiChannel, MidiGroup, System, Ump};

use super::mpe_ingest::MpeIngest;
use super::order::{BLANK, in_offset_order, offset_of};
use super::sensing::ActiveSensing;
use crate::held::HeldState;
use crate::live::device_inlets::{DeviceInlets, DeviceSlot};

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
    pub(crate) const fn last_group(self) -> MidiGroup {
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
/// use std::num::NonZeroUsize;
/// use tutti_midi_runtime::MidiIngestNode;
///
/// let (mut editor, _exec) = Editor::new(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples(512))));
/// // Four device slots, none wired yet.
/// let controls = editor.insert(NodeKey(1), "midi in", MidiIngestNode::new(NonZeroUsize::new(4).unwrap()));
/// controls.set_mpe_mode(tutti_midi_protocol::MpeMode::Disabled);
/// ```
pub struct MidiIngestNode {
    /// One translation state per device slot.
    devices: Box<[Device]>,
    /// How many `devices` there are: never zero.
    count: NonZeroUsize,
    /// The slots' epochs ([`MidiIngestNode::following`]); `None` for a node
    /// whose slots no inlets hand out, which never restarts one.
    epochs: Option<Arc<[AtomicU64]>>,
    /// What the block sends: the translated events, which may outnumber the
    /// ones read (a member channel's bend range fans out over its zone).
    out: Box<[UmpAt]>,
    /// The Groups it hears, and so its ports.
    ports: MidiInputPorts,
    /// The MPE mode in force on every slot.
    mpe_mode: MpeMode,
    /// What a host asked for, read once per block. The reader owns its cell
    /// (`Reader::cell`), which the controls publish to.
    mpe_request: Reader<Requests>,
    /// The Profile zones in force on each slot's Groups: what the requests
    /// last read set.
    profiles: Box<[GroupProfiles]>,
    /// The generation of the requests `profiles` was read from.
    profiles_generation: u64,
    /// Translated events a block had no room for in `out`
    /// ([`MidiIngestControls::losses`]).
    dropped: Arc<AtomicU64>,
    /// The events a block translates and each port holds, from the block the
    /// node was prepared for.
    capacity: usize,
}

/// The MPE Profile zones of each of a device's Groups.
type GroupProfiles = [Option<MpeZones>; MidiGroup::COUNT as usize];

/// What the control side asks of the node, published whole.
#[derive(Clone, Debug)]
struct Requests {
    /// The MPE 1.1 mode for every slot, if one was asked for.
    mode: Option<MpeMode>,
    /// The MPE Profile zones each slot's devices enabled.
    profiles: Box<[GroupProfiles]>,
    /// Counts the changes of `profiles`, so the node reads them only when
    /// there is one.
    generation: u64,
}

impl Requests {
    fn new(slots: NonZeroUsize) -> Self {
        Self {
            mode: None,
            profiles: no_profiles(slots),
            generation: 0,
        }
    }
}

fn no_profiles(slots: NonZeroUsize) -> Box<[GroupProfiles]> {
    vec![[None; MidiGroup::COUNT as usize]; slots.get()].into_boxed_slice()
}

/// What one device slot keeps between blocks: its MIDI 1.0 assembler and its
/// classic-MPE state.
struct Device {
    translator: Translator,
    mpe: MpeIngest,
    /// The slot's epoch this state was last reset at.
    epoch: u64,
    /// Whether a device holds the slot: an event of one that does not is not
    /// read. Always for a node that follows no inlets.
    present: bool,
    /// What the device started and has not ended, as it sent it.
    held: HeldState,
    /// The device's Groups that have sent an Active Sensing.
    sensing: ActiveSensing,
    /// The Groups (bit `g` for Group `g`) whose release did not fit the
    /// block's room: tried again at the next block's first frame.
    owing: u16,
}

impl Device {
    fn new(mode: MpeMode) -> Self {
        Self {
            translator: Translator::new(),
            mpe: MpeIngest::new(mode),
            epoch: 0,
            present: true,
            held: HeldState::new(),
            // Built again for the real rate in `prepare`.
            sensing: ActiveSensing::at_rate(SampleRate::SR_48K),
            owing: 0,
        }
    }

    /// Empty, as for a device that has not sent anything. What the old device
    /// still holds is not forgotten: it was released first, and what did not
    /// fit is owed.
    fn restart(&mut self, mode: MpeMode, epoch: u64) {
        self.translator.reset();
        self.mpe = MpeIngest::new(mode);
        self.sensing.forget_all();
        self.epoch = epoch;
        // A device holds the slot while its epoch is odd.
        self.present = epoch % 2 == 1;
    }

    /// Releases what `group` (every Group for `None`) held, at `at`, through
    /// the translation as the device's own messages would go, into `staging`.
    /// What the block has no room for stays held and owed.
    fn release(&mut self, group: Option<MidiGroup>, at: Offset, staging: &mut Staging<'_>) {
        let Self {
            translator,
            mpe,
            held,
            owing,
            ..
        } = self;
        let all = held.release(group, |raw| {
            let Some(ev) = translator.translate(raw) else {
                return true;
            };
            staging.whole(ev.is_note_off(), |send| mpe.translate(&(at, ev), send))
        });
        let mask = group.map_or(u16::MAX, |g| 1 << g.get());
        if all {
            *owing &= !mask;
        } else {
            *owing |= mask;
        }
    }

    /// Ends what each Group that ran out of Active Sensing time by `now` held
    /// (see the module docs), at the frame it ran out, in the block that
    /// starts at `first` and is `block_len` long.
    fn lapse(&mut self, now: Frame, first: Frame, block_len: Samples, staging: &mut Staging<'_>) {
        while let Some((group, ran_out)) = self.sensing.next_lapse(now) {
            let at = ran_out.since(first).unwrap_or(Samples::ZERO);
            let at = Offset::new(at, block_len).unwrap_or(Offset::last(block_len));
            self.release(Some(group), at, staging);
        }
    }

    /// A System Reset on `group`'s cable at `at` (see the module docs).
    fn reset_group(&mut self, group: MidiGroup, at: Offset, staging: &mut Staging<'_>) {
        self.release(Some(group), at, staging);
        self.translator.reset_group(group);
        self.mpe.reset_group(group);
        self.sensing.forget(group);
    }

    /// The release of every Group that owes one.
    fn pay(&mut self, staging: &mut Staging<'_>) {
        for group in MidiGroup::all() {
            if self.owing & (1 << group.get()) != 0 {
                self.release(Some(group), Offset::ZERO, staging);
            }
        }
    }
}

impl std::fmt::Debug for MidiIngestNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiIngestNode")
            .field("devices", &self.devices.len())
            .field("mpe_mode", &self.mpe_mode)
            .field("mpe_request", &self.mpe_request)
            .field("profiles", &self.profiles)
            .field("dropped", &self.dropped)
            .finish_non_exhaustive()
    }
}

impl MidiIngestNode {
    /// A node with `devices` event inputs, one per device slot,
    /// with MPE ingestion off.
    #[must_use]
    pub fn new(devices: NonZeroUsize) -> Self {
        let cell = Arc::new(ArcLend::new(Requests::new(devices)));
        Self {
            mpe_request: cell.reader(),
            count: devices,
            devices: (0..devices.get())
                .map(|_| Device::new(MpeMode::Disabled))
                .collect(),
            profiles: no_profiles(devices),
            profiles_generation: 0,
            epochs: None,
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
    pub fn devices(&self) -> NonZeroUsize {
        self.count
    }

    /// Restart a slot's translator and MPE state when a device takes it, as
    /// `inlets` hand the slots out: the node reads each slot's epoch once per
    /// block (a relaxed load of a cell the control side stores to when a
    /// device opens), so no one has to tell it. The slots are the inlets' own,
    /// so the node must have as many.
    ///
    /// # Errors
    ///
    /// [`SlotCountMismatch`] when the node and the inlets have different
    /// numbers of slots.
    pub fn following(mut self, inlets: &DeviceInlets) -> Result<Self, SlotCountMismatch> {
        if inlets.slots() != self.count {
            return Err(SlotCountMismatch {
                node: self.count,
                inlets: inlets.slots(),
            });
        }
        self.epochs = Some(inlets.epochs());
        Ok(self)
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
        let Some(mode) = self.mpe_request.lend().mode else {
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

/// A [`MidiIngestNode`] and the [`DeviceInlets`] it was to follow have
/// different numbers of slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the ingest node has {} device slots and the inlets {}", .node, .inlets)]
pub struct SlotCountMismatch {
    /// The node's event inputs.
    pub node: NonZeroUsize,
    /// The inlets' slots.
    pub inlets: NonZeroUsize,
}

/// The host's handle on a [`MidiIngestNode`]: its MPE mode and the MPE Profile
/// zones of its devices. Cheap to clone.
#[derive(Clone, Debug)]
pub struct MidiIngestControls {
    /// What the node reads; the controls publish to it and read it back with
    /// `ArcLend::current`.
    request: Arc<ArcLend<Requests>>,
    dropped: Arc<AtomicU64>,
}

impl MidiIngestControls {
    /// Ingest classic MPE in `mode` from the next block. Control thread.
    ///
    /// A new mode stops and resets the channels that enter or leave MPE
    /// control (M1-100 §2.2.3: All Sound Off and Reset All Controllers, sent
    /// ahead of the new zones' default bend ranges); the others keep their
    /// notes. Setting the mode in force changes nothing.
    pub fn set_mpe_mode(&self, mode: MpeMode) {
        let mut request = (*self.request.current()).clone();
        request.mode = Some(mode);
        self.request.publish(request);
    }

    /// The MIDI-CI MPE Profile zones (M2-120) the device holding `slot` enabled
    /// on `group`: from the next block, what the device sends on that Group
    /// follows the zones' rules instead of the mode's, until `zones` is `None`
    /// ([`MpeIngest::set_profile`] says what the change stops and resets).
    /// Control thread. Setting what is set changes nothing.
    ///
    /// The zones are the device's alone: another slot's, and the mode
    /// [`set_mpe_mode`](Self::set_mpe_mode) sets, are not changed. A slot a
    /// new device takes starts with none, but the control side's record stays
    /// until it is set: clear it when the device goes
    /// ([`clear_mpe_profiles`](Self::clear_mpe_profiles)).
    ///
    /// # Panics
    ///
    /// If `slot` is not one of the inlets the node follows
    /// ([`MidiIngestNode::following`]): those have as many slots as the node,
    /// so each slot they hand out is one of its event inputs.
    pub fn set_mpe_profile(&self, slot: DeviceSlot, group: MidiGroup, zones: Option<MpeZones>) {
        self.edit_profiles(slot, |held| {
            let changed = held[group.index()] != zones;
            held[group.index()] = zones;
            changed
        });
    }

    /// [`set_mpe_profile`](Self::set_mpe_profile) with none, on every Group of
    /// `slot`.
    ///
    /// # Panics
    ///
    /// As [`set_mpe_profile`](Self::set_mpe_profile).
    pub fn clear_mpe_profiles(&self, slot: DeviceSlot) {
        self.edit_profiles(slot, |held| {
            let changed = held.iter().any(Option::is_some);
            *held = [None; MidiGroup::COUNT as usize];
            changed
        });
    }

    /// Publishes the requests with `edit` applied to the slot's zones, when it
    /// says it changed them.
    fn edit_profiles(&self, slot: DeviceSlot, edit: impl FnOnce(&mut GroupProfiles) -> bool) {
        let mut request = (*self.request.current()).clone();
        let held = &mut request.profiles[slot.index()];
        if edit(held) {
            request.generation += 1;
            self.request.publish(request);
        }
    }

    /// What the node has lost since it was built, in events: all of it
    /// `refused`, the translated events that found no room past the node's
    /// capacity in one block. An event's
    /// translation (a bend range or an MPE reset fans out over a zone) is
    /// dropped whole rather than cut partway. What a device's inlet could not
    /// hold is counted at the inlet, not here
    /// ([`DeviceInlets`]).
    pub fn losses(&self) -> Losses {
        Losses::refused(self.dropped.load(Ordering::Relaxed))
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
        for device in &mut self.devices {
            // The limit is frames of this rate; a watch counted at another is
            // dropped, and the device's next Active Sensing starts it again.
            device.sensing = ActiveSensing::at_rate(p.sample_rate());
        }
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
        let block_len = cx.env.block_len;
        let first = cx.env.frame;
        // An event's translation goes in whole or not at all: a fan-out cut
        // partway would leave some of a zone's channels at another range.
        // A note-off may use the room past the capacity (the port's own
        // `note_off_room`), so a block full of other messages (a bend range
        // fanned out over a zone) cannot keep it out.
        let mut staging = Staging::new(&mut self.out, self.capacity);
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
        // The Profile zones a host set, at frame 0 too: each device's change
        // stops and resets the channels it moves, and starts the zones at their
        // bend ranges.
        {
            let requests = self.mpe_request.lend();
            if requests.generation != self.profiles_generation {
                self.profiles_generation = requests.generation;
                for (slot, device) in self.devices.iter_mut().enumerate() {
                    let Some(wants) = requests.profiles.get(slot) else {
                        continue;
                    };
                    for group in MidiGroup::all() {
                        let want = wants[group.index()];
                        let held = &mut self.profiles[slot][group.index()];
                        if want != *held {
                            *held = want;
                            staging.whole(false, |send| {
                                device.mpe.set_profile(group, want, Offset::ZERO, send);
                            });
                        }
                    }
                }
            }
        }
        for (slot, device) in self.devices.iter_mut().enumerate() {
            // A device that took the slot or let go of it: what the one before
            // held is ended before anything else of the slot is read.
            if let Some(epochs) = &self.epochs {
                let epoch = epochs[slot].load(Ordering::Acquire);
                if epoch != device.epoch {
                    device.release(None, Offset::ZERO, &mut staging);
                    device.restart(mode, epoch);
                    // The new device has enabled no Profile.
                    self.profiles[slot] = [None; MidiGroup::COUNT as usize];
                }
            }
            device.pay(&mut staging);
            if !device.present {
                continue;
            }
            for event in io.events(slot) {
                let EventKind::Midi(raw) = event.kind else {
                    continue;
                };
                let at = event.offset;
                let frame = cx.env.frame_at(at);
                // A cable that ran out of time before this message ended its
                // notes then, not now.
                device.lapse(frame, first, block_len, &mut staging);
                let message = raw.decode().ok();
                if let Some(group) = raw.group() {
                    let sensing =
                        matches!(message, Some(Message::System(_, System::ActiveSensing)));
                    device.sensing.heard(group, frame, sensing);
                }
                match message {
                    // The cable's own: acted on here, sent on to no one.
                    Some(Message::System(_, System::ActiveSensing)) => continue,
                    Some(Message::System(group, System::Reset)) => {
                        device.reset_group(group, at, &mut staging);
                        continue;
                    }
                    _ => {}
                }
                device.held.observe(raw);
                if let Some(ev) = device.translator.translate(raw) {
                    let mpe = &mut device.mpe;
                    staging.whole(ev.is_note_off(), |send| mpe.translate(&(at, ev), send));
                }
            }
            device.lapse(first + block_len, first, block_len, &mut staging);
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
pub(super) struct Staging<'a> {
    out: &'a mut [UmpAt],
    capacity: usize,
    /// How many of `out` are kept.
    pub(super) kept: usize,
    /// How many events were dropped for want of room.
    pub(super) dropped: u64,
}

impl<'a> Staging<'a> {
    /// Staging into `out`, of which a note-off may use all and any other
    /// event the first `capacity`.
    pub(super) fn new(out: &'a mut [UmpAt], capacity: usize) -> Self {
        Self {
            out,
            capacity,
            kept: 0,
            dropped: 0,
        }
    }
}

impl Staging<'_> {
    /// Keeps everything `make` sends, or, if it does not all fit, none of it
    /// and counts it dropped; whether it all fit. A `note_off`'s may use the
    /// room past the capacity.
    pub(super) fn whole(
        &mut self,
        note_off: bool,
        make: impl FnOnce(&mut dyn FnMut(UmpAt)),
    ) -> bool {
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
        fits
    }
}

/// A [`MidiIngestNode`]'s fork: the same slots and ports, nothing to read.
struct SilentInput {
    ports: MidiInputPorts,
    devices: NonZeroUsize,
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
            devices: self.count,
        };
        let controls = MidiIngestControls {
            request: Arc::clone(self.mpe_request.cell()),
            dropped: Arc::clone(&self.dropped),
        };
        NodeParts {
            node: Box::new(self),
            controls,
            fork: Some(Box::new(fork)),
        }
    }
}

/// Frees what the control side retired and no block holds, without publishing
/// (the MPE requests a replacement left behind); control thread, from the owner's tick.
impl tutti_rt::Reclaim for MidiIngestControls {
    fn reclaim(&self) -> usize {
        self.request.reclaim()
    }
}

#[cfg(test)]
mod tests;
