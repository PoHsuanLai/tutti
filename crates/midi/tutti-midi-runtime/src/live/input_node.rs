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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tutti_core::{ArcLend, ChannelLayout, Reader};
use tutti_graph::{
    Cx, Event, ForkCause, ForkMode, ForkSource, Forked, IntoNode, Io, Node, NodeParts, Prepare,
    Shape, Status,
};
use tutti_midi_types::mpe::MpeMode;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{Midi1ToMidi2Translator, MidiChannel, MidiGroup, MidiIn};

use super::order::{in_offset_order, offset_of};
use crate::outbound::mpe_ingest::{MpeIngest, Translated};

/// The port that carries events with no channel: after the sixteen channel
/// ports.
pub const CHANNELLESS_PORT: usize = MidiChannel::COUNT as usize;

/// How many event outputs a [`MidiInputNode`] has: one per channel and
/// [`CHANNELLESS_PORT`].
pub const MIDI_INPUT_PORTS: usize = CHANNELLESS_PORT + 1;

/// The most events a [`MidiInputNode`] polls in one block, and each port's
/// declared event capacity: past it the rest are dropped.
pub const MIDI_INPUT_EVENT_CAPACITY: usize = 512;

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
/// use tutti_core::{NodeKey, SampleRate, Samples};
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
    request: Arc<MpeRequest>,
    /// A reader of `request.cell`, registered in [`new`](Self::new).
    mpe_request: Reader<Option<MpeMode>>,
    /// Translated events a block had no room for in `out`
    /// ([`MidiInputControls::dropped_events`]).
    dropped: Arc<AtomicU64>,
}

/// The MPE mode a host asked for: what the node reads, and the control
/// side's record of it.
struct MpeRequest {
    /// What the node reads, once per block, through its reader.
    cell: Arc<ArcLend<Option<MpeMode>>>,
    /// The mode last asked for, for the control side
    /// ([`MidiInputControls::pending_mpe_mode`]). The node never takes this
    /// lock.
    asked: Mutex<Option<MpeMode>>,
}

impl MidiInputNode {
    /// A node polling `input` each block (`None`: a node that sends nothing),
    /// with MPE ingestion off. The node owns `input`: build one per node (for
    /// the hardware ports, `tutti_midi_hardware::HardwareMidiDrain::new`
    /// over the shared `Arc<HardwareMidiInputs>`).
    pub fn new(input: Option<Box<dyn MidiIn>>) -> Self {
        let request = Arc::new(MpeRequest {
            cell: Arc::new(ArcLend::new(None)),
            asked: Mutex::new(None),
        });
        Self {
            mpe_request: request.cell.reader(),
            input,
            scratch: vec![MidiEvent::noop(); MIDI_INPUT_EVENT_CAPACITY].into_boxed_slice(),
            out: vec![MidiEvent::noop(); MIDI_INPUT_EVENT_CAPACITY].into_boxed_slice(),
            translator: Midi1ToMidi2Translator::new(),
            mpe: MpeIngest::new(MpeMode::Disabled),
            request,
            dropped: Arc::new(AtomicU64::new(0)),
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
#[derive(Clone)]
pub struct MidiInputControls {
    request: Arc<MpeRequest>,
    dropped: Arc<AtomicU64>,
}

impl MidiInputControls {
    /// Ingest classic MPE in `mode` from the next block. Control thread.
    ///
    /// A new mode resets MPE voice allocation: a note sounding through a
    /// member channel loses its mapping. Setting the mode in force changes
    /// nothing.
    pub fn set_mpe_mode(&self, mode: MpeMode) {
        let mut asked = self.asked();
        *asked = Some(mode);
        self.request.cell.publish(Arc::new(Some(mode)));
    }

    /// Frees the requests the node has let go of since the last
    /// [`set_mpe_mode`](Self::set_mpe_mode), without publishing; returns how
    /// many are still held. Control thread, from the owner's tick (a
    /// session's `Reclaim` phase sweeps its hardware input's); with nothing
    /// retired it is a lock and a length check.
    pub fn reclaim(&self) -> usize {
        self.request.cell.reclaim()
    }

    /// How many translated events the node has dropped since it was built:
    /// past [`MIDI_INPUT_EVENT_CAPACITY`] in one block, an event's
    /// translation (a bend range or an MPE reset fans out over a zone) is
    /// dropped whole rather than cut partway. A poll past the capacity is
    /// the wire's own truncation, not counted here.
    pub fn dropped_events(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// The mode last asked for through [`set_mpe_mode`](Self::set_mpe_mode),
    /// if any: what was asked, not what is in force (they differ for a block
    /// after a change, and for good if the graph never runs). Control side:
    /// reads its own record, never the node's cell.
    pub fn pending_mpe_mode(&self) -> Option<MpeMode> {
        *self.asked()
    }

    /// The control side's record, under its lock: held across a publish so
    /// two `set_mpe_mode` calls cannot leave the record and the cell
    /// disagreeing.
    fn asked(&self) -> std::sync::MutexGuard<'_, Option<MpeMode>> {
        // A poisoned lock still holds a whole `Option`, set in one write.
        self.request
            .asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl Node for MidiInputNode {
    /// No audio; [`MIDI_INPUT_PORTS`] event outputs of
    /// [`MIDI_INPUT_EVENT_CAPACITY`].
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
            .with_events(0, MIDI_INPUT_PORTS as u16)
            .with_event_capacity(MIDI_INPUT_EVENT_CAPACITY as u32)
            // Event-only: a poll and a translation per event, nothing per
            // frame.
            .with_cost(tutti_graph::Cost::Light)
    }

    fn prepare(&mut self, _p: &Prepare) {}

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
        let mut stage = |events: Translated| {
            let n = events.len();
            match self.out.get_mut(kept..kept + n) {
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
            stage(reset);
        }
        let n = input.poll_block(block_len.get(), &mut self.scratch);
        for raw in &self.scratch[..n.min(self.scratch.len())] {
            let Some(ev) = self.translator.translate(raw) else {
                continue;
            };
            stage(self.mpe.translate(&ev));
        }
        if dropped > 0 {
            self.dropped.fetch_add(dropped, Ordering::Relaxed);
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
            request: Arc::clone(&self.request),
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
mod tests {
    use super::*;

    /// The controls report the mode last asked for, from their own record,
    /// and the node adopts it on its next block.
    ///
    /// Mutation (run): `set_mpe_mode` publishing without writing the record
    /// (drop `*asked = Some(mode)`) → `pending_mpe_mode` stays `None`,
    /// fails. The node built without a reader on the controls' cell (its
    /// `mpe_request` left registered on a cell of its own) → it never sees
    /// the request, fails (run).
    #[test]
    fn the_controls_report_the_mode_last_asked_for() {
        use tutti_midi_types::mpe::MpeZoneConfig;
        let parts = MidiInputNode::new(None).into_parts();
        let controls = parts.controls;
        assert_eq!(controls.pending_mpe_mode(), None);
        let lower = MpeMode::LowerZone(MpeZoneConfig::lower(15));
        let upper = MpeMode::UpperZone(MpeZoneConfig::upper(7));
        controls.set_mpe_mode(lower);
        controls.clone().set_mpe_mode(upper);
        assert_eq!(controls.pending_mpe_mode(), Some(upper), "the last asked");

        // A node over the same request, reading it through a reader of its
        // own, as the inserted one is.
        let mut node = MidiInputNode {
            mpe_request: controls.request.cell.reader(),
            request: Arc::clone(&controls.request),
            ..MidiInputNode::new(None)
        };
        node.adopt_mpe_request();
        assert_eq!(*node.mpe.mode(), upper, "the node adopts it");
    }

    /// **A request the node held across the next `set_mpe_mode` is freed by
    /// the next `reclaim`**, with no further publish.
    ///
    /// Mutation (run): `MidiInputControls::reclaim` returning 0 without
    /// reclaiming → one request is still retired, fails.
    #[test]
    fn reclaim_frees_a_request_a_block_held_across_the_next() {
        use tutti_midi_types::mpe::MpeZoneConfig;
        let mut node = MidiInputNode::new(None);
        let controls = MidiInputControls {
            request: Arc::clone(&node.request),
            dropped: Arc::clone(&node.dropped),
        };
        {
            // A block in progress across the request.
            let _held = node.mpe_request.lend();
            controls.set_mpe_mode(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
        }
        assert_eq!(node.request.cell.retired_len(), 1, "pinned until a scan");
        assert_eq!(controls.reclaim(), 0);
        assert_eq!(node.request.cell.retired_len(), 0, "freed by the reclaim");
    }

    /// A listener on one channel takes that channel's port and the
    /// channelless one; an omni listener takes all seventeen.
    ///
    /// Mutation: leave out the channelless port for a channel → fails.
    #[test]
    fn a_listener_takes_its_channel_and_the_channelless_port() {
        let one: Vec<usize> = input_ports(Some(MidiChannel::new(3))).collect();
        assert_eq!(one, vec![3, CHANNELLESS_PORT]);
        let all: Vec<usize> = input_ports(None).collect();
        assert_eq!(all, (0..MIDI_INPUT_PORTS).collect::<Vec<_>>());
    }

    /// A voice message leaves on its channel's port; a system message on
    /// the channelless one.
    #[test]
    fn events_leave_on_their_channels_port() {
        let on = MidiEvent::note_on_7bit(
            tutti_midi_types::MidiGroup::FIRST,
            MidiChannel::new(9),
            60,
            100,
        );
        assert_eq!(input_port_of(&on), 9);
        let clock = MidiEvent::timing_clock(tutti_midi_types::MidiGroup::FIRST);
        assert_eq!(input_port_of(&clock), CHANNELLESS_PORT);
    }
}
