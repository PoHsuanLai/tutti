//! MIDI wire types for the host↔server IPC channel.

use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use tutti_midi_types::ump::MidiEvent;

pub(super) const MIDI_STACK_CAPACITY: usize = 256;

/// On-wire MIDI event. 20 bytes — packs the full UMP event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcMidiEvent {
    pub frame_offset: u32,
    pub data: [u32; 4],
}

impl IpcMidiEvent {
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

pub type IpcMidiEventVec = SmallVec<[IpcMidiEvent; MIDI_STACK_CAPACITY]>;
pub type MidiEventVec = SmallVec<[MidiEvent; MIDI_STACK_CAPACITY]>;
