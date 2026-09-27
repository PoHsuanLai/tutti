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

use tutti_core::{ChannelLayout, RtPublish};
use tutti_graph::{
    Cx, Event, ForkCause, ForkMode, ForkSource, Forked, IntoNode, Io, Node, NodeParts, Prepare,
    Shape, Status,
};
use tutti_midi_types::mpe::MpeMode;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{Midi1ToMidi2Translator, MidiChannel, MidiIn};

use super::order::{in_offset_order, offset_of};
use crate::outbound::mpe_ingest::MpeIngest;

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
    input: Option<Arc<dyn MidiIn>>,
    scratch: Box<[MidiEvent]>,
    translator: Midi1ToMidi2Translator,
    mpe: MpeIngest,
    request: Arc<RtPublish<Option<MpeMode>>>,
}

impl MidiInputNode {
    /// A node polling `input` each block (`None`: a node that sends nothing),
    /// with MPE ingestion off.
    pub fn new(input: Option<Arc<dyn MidiIn>>) -> Self {
        Self {
            input,
            scratch: vec![MidiEvent::noop(); MIDI_INPUT_EVENT_CAPACITY].into_boxed_slice(),
            translator: Midi1ToMidi2Translator::new(),
            mpe: MpeIngest::new(MpeMode::Disabled),
            request: Arc::new(RtPublish::new(None)),
        }
    }

    /// Ingest classic MPE in `mode` from the first block.
    #[must_use]
    pub fn with_mpe(mut self, mode: MpeMode) -> Self {
        self.mpe = MpeIngest::new(mode);
        self
    }

    /// Adopt a requested MPE mode that differs from the one in force.
    ///
    /// **The comparison is load-bearing**: the request latches (the audio
    /// thread cannot clear it without publishing), and `set_mode` rebuilds
    /// the ingest, so re-adopting it every block would drop every sounding
    /// note's voice mapping every block.
    fn adopt_mpe_request(&mut self) {
        let pending = *self.request.read();
        if let Some(mode) = pending {
            if *self.mpe.mode() != mode {
                self.mpe.set_mode(mode);
            }
        }
    }
}

/// The host's handle on a [`MidiInputNode`]: its MPE mode. Cheap to clone.
#[derive(Clone)]
pub struct MidiInputControls {
    request: Arc<RtPublish<Option<MpeMode>>>,
}

impl MidiInputControls {
    /// Ingest classic MPE in `mode` from the next block. Control thread.
    ///
    /// A new mode resets MPE voice allocation: a note sounding through a
    /// member channel loses its mapping. Setting the mode in force changes
    /// nothing.
    pub fn set_mpe_mode(&self, mode: MpeMode) {
        self.request.publish(Arc::new(Some(mode)));
    }

    /// The mode last asked for through [`set_mpe_mode`](Self::set_mpe_mode),
    /// if any: what was asked, not what is in force (they differ for a block
    /// after a change, and for good if the graph never runs).
    pub fn pending_mpe_mode(&self) -> Option<MpeMode> {
        *self.request.read()
    }
}

impl Node for MidiInputNode {
    /// No audio; [`MIDI_INPUT_PORTS`] event outputs of
    /// [`MIDI_INPUT_EVENT_CAPACITY`].
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
            .with_events(0, MIDI_INPUT_PORTS as u16)
            .with_event_capacity(MIDI_INPUT_EVENT_CAPACITY as u32)
    }

    fn prepare(&mut self, _p: &Prepare) {}

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        // Before any event is translated, stated or not.
        self.adopt_mpe_request();
        let Some(input) = &self.input else {
            return Status::Modified;
        };
        let block_len = cx.env.block_len;
        let n = input.poll_block(block_len.get(), &mut self.scratch);
        let mut kept = 0;
        for i in 0..n.min(self.scratch.len()) {
            let raw = self.scratch[i];
            let Some(ev) = self.translator.translate(&raw) else {
                continue;
            };
            if let Some(ev) = self.mpe.translate(&ev) {
                self.scratch[kept] = ev;
                kept += 1;
            }
        }
        let events = &mut self.scratch[..kept];
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
    fn fork(&self, _mode: ForkMode<'_>) -> Result<Forked, ForkCause> {
        Ok(Forked::new(Box::new(MidiInputNode::new(None))))
    }
}

impl IntoNode for MidiInputNode {
    type Controls = MidiInputControls;

    fn into_parts(self) -> NodeParts<MidiInputControls> {
        let controls = MidiInputControls {
            request: Arc::clone(&self.request),
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
