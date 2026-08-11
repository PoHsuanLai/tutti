//! MIDI wire types for the host↔server IPC channel.

use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use tutti_midi_types::ump::MidiEvent;

/// Events a MIDI block holds inline before spilling to the heap.
///
/// Sized so the common case — zero to a handful of events per block — never
/// allocates on the RT-adjacent path. The subprocess caps its outgoing MIDI at
/// this count for the same reason.
pub const MIDI_STACK_CAPACITY: usize = 256;

/// On-wire MIDI event. 20 bytes — packs the full UMP event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcMidiEvent {
    /// Position within the block, in **frames** from its start.
    pub frame_offset: u32,
    /// The UMP packet, up to four 32-bit words.
    pub data: [u32; 4],
}

impl IpcMidiEvent {
    /// Converts to the engine's [`MidiEvent`], which is the same data off-wire.
    pub fn to_midi_event(&self) -> MidiEvent {
        MidiEvent {
            frame_offset: self.frame_offset,
            data: self.data,
        }
    }
}

impl From<MidiEvent> for IpcMidiEvent {
    fn from(event: MidiEvent) -> Self {
        Self {
            frame_offset: event.frame_offset,
            data: event.data,
        }
    }
}

impl From<&MidiEvent> for IpcMidiEvent {
    fn from(event: &MidiEvent) -> Self {
        Self::from(*event)
    }
}

impl From<IpcMidiEvent> for MidiEvent {
    fn from(ipc: IpcMidiEvent) -> Self {
        ipc.to_midi_event()
    }
}

/// A block's worth of wire MIDI, inline up to [`MIDI_STACK_CAPACITY`].
pub type IpcMidiEventVec = SmallVec<[IpcMidiEvent; MIDI_STACK_CAPACITY]>;

/// A block's worth of engine MIDI, inline up to [`MIDI_STACK_CAPACITY`].
pub type MidiEventVec = SmallVec<[MidiEvent; MIDI_STACK_CAPACITY]>;
