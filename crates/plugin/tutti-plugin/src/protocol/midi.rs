//! MIDI wire types for the host↔server IPC channel.

use serde::{Deserialize, Serialize};
use tutti_midi_types::ump::MidiEvent;

/// The engine-side block MIDI list and its cap, shared with the format hosts:
/// one type, so a block's MIDI moves between them without a copy. Each
/// direction of the wire carries at most this many events a block; the
/// subprocess caps its outgoing MIDI at it.
pub use tutti_plugin_types::{MidiEventVec, MIDI_STACK_CAPACITY};

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

/// A block's worth of wire MIDI.
///
/// A plain `Vec`: it is built and read only off the audio thread (the host's
/// bridge thread, and the subprocess's message loop, which encodes every reply
/// into a fresh buffer anyway), so storage inline buys nothing. Its producers
/// cap it at [`MIDI_STACK_CAPACITY`]; a decoded one is not capped, and the
/// receiver converts at most that many ([`MidiEventVec`] cannot hold more).
pub type IpcMidiEventVec = Vec<IpcMidiEvent>;

/// A block's wire MIDI as the engine's list: the first
/// [`MIDI_STACK_CAPACITY`] events, converted, and the rest dropped.
///
/// The one door from the wire to a [`MidiEventVec`], on both sides of it (the
/// host reading a reply's MIDI-out, the subprocess reading a block's MIDI-in).
/// A decoded [`IpcMidiEventVec`] has whatever length the peer sent, and a
/// `MidiEventVec` cannot hold more than its cap (`collect` would panic), so
/// the excess is dropped here, as a full list drops it everywhere else.
pub fn midi_from_wire(events: &[IpcMidiEvent]) -> MidiEventVec {
    events
        .iter()
        .take(MIDI_STACK_CAPACITY)
        .map(IpcMidiEvent::to_midi_event)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(n: u32) -> IpcMidiEventVec {
        (0..n)
            .map(|i| IpcMidiEvent {
                frame_offset: i,
                data: [0x2090_3C40, 0, 0, 0],
            })
            .collect()
    }

    /// **A peer's list past the cap converts to the first
    /// `MIDI_STACK_CAPACITY`, without panicking.** The wire list is a `Vec`,
    /// so nothing bounds what a peer sends.
    ///
    /// Mutation (run): the `take` removed → `collect` panics past the cap →
    /// fails.
    #[test]
    fn a_long_wire_list_keeps_the_first_events() {
        let got = midi_from_wire(&wire(MIDI_STACK_CAPACITY as u32 + 50));
        assert_eq!(got.len(), MIDI_STACK_CAPACITY);
        assert_eq!(got[0].frame_offset, 0);
        assert_eq!(got[MIDI_STACK_CAPACITY - 1].frame_offset, 255);
        assert_eq!(midi_from_wire(&wire(3)).len(), 3);
    }

    /// **The wire list's format is unchanged**: a sequence (a length, then the
    /// events), as the inline list it replaced encoded, so a host and a
    /// subprocess built on either side of the change read each other.
    ///
    /// No change to the list type that keeps it a list changes these bytes;
    /// the layout is written out instead. Mutation (run): the expected bytes
    /// without the length prefix → fails.
    #[test]
    fn the_wire_list_encodes_as_a_sequence() {
        let events = wire(5);
        let mut expected = (events.len() as u64).to_le_bytes().to_vec();
        for e in &events {
            expected.extend(bincode::serialize(e).expect("serialize"));
        }
        assert_eq!(bincode::serialize(&events).expect("serialize"), expected);
    }
}
