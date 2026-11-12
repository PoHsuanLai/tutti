//! [`MidiKeys`]: the key helpers over an event inlet's writer.
//!
//! Copying events into the graph is `tutti-graph`'s event inlet (an
//! [`EventInletNode`](tutti_graph::EventInletNode) and its
//! [`EventInletWriter`]); the graph builds no message. What a keyboard, a
//! preview or an all-notes-off needs on top of it is a Note On and a Note Off
//! built as MIDI 2.0 packets on the Group the keys were made for, and that is
//! all this is: the writer is borrowed through `Deref`, so `push`, `push_all`,
//! `clock` and the counts are the inlet's own.

use std::ops::{Deref, DerefMut};

use tutti_graph::{EventInletWriter, Refused, Stamp};
use tutti_ump::{Address, Message, MidiChannel, MidiGroup, NoteNumber, Ump, Voice2};

/// The ring a live input's inlet is opened with, in events (it is also the
/// waiting list's size and the port's declared capacity).
pub const LIVE_INLET_CAPACITY: usize = 256;

/// An [`EventInletWriter`] with the key helpers. One producer, as the writer
/// is: share it behind a lock on the control side if it must be `Clone`.
#[derive(Debug)]
pub struct MidiKeys {
    writer: EventInletWriter,
    group: MidiGroup,
}

impl MidiKeys {
    /// The helpers over `writer` (from `Graph::open_event_inlet` or
    /// `EventInletNode::pair`, opened with [`LIVE_INLET_CAPACITY`]), sending on
    /// `group`: the one Group of sixteen channels an instrument plays
    /// (`MidiGroup::FIRST` for the first).
    #[must_use]
    pub fn new(writer: EventInletWriter, group: MidiGroup) -> Self {
        Self { writer, group }
    }

    /// The Group every key helper sends on.
    #[must_use]
    pub fn group(&self) -> MidiGroup {
        self.group
    }

    /// A MIDI 2.0 Note On at `velocity` (16 bits) on the keys' Group, for
    /// `at`. A velocity of 0 is a Note On (M2-104 §7.4.2).
    ///
    /// # Errors
    ///
    /// As [`EventInletWriter::push`].
    pub fn note_on(
        &mut self,
        at: impl Into<Stamp>,
        channel: MidiChannel,
        note: NoteNumber,
        velocity: u16,
    ) -> Result<(), Refused> {
        self.voice(at, channel, Voice2::note_on(note, velocity))
    }

    /// A MIDI 2.0 Note Off at release velocity 0 on the keys' Group, for `at`. A full
    /// ring refuses it, which would leave the note stuck.
    ///
    /// # Errors
    ///
    /// As [`EventInletWriter::push`].
    pub fn note_off(
        &mut self,
        at: impl Into<Stamp>,
        channel: MidiChannel,
        note: NoteNumber,
    ) -> Result<(), Refused> {
        self.voice(at, channel, Voice2::note_off(note, 0))
    }

    fn voice(
        &mut self,
        at: impl Into<Stamp>,
        channel: MidiChannel,
        voice: Voice2,
    ) -> Result<(), Refused> {
        let address = Address {
            group: self.group,
            channel,
        };
        self.writer
            .push(at, Ump::from(Message::Midi2(address, voice)))
    }
}

impl Deref for MidiKeys {
    type Target = EventInletWriter;
    fn deref(&self) -> &EventInletWriter {
        &self.writer
    }
}

impl DerefMut for MidiKeys {
    fn deref_mut(&mut self) -> &mut EventInletWriter {
        &mut self.writer
    }
}

#[cfg(test)]
mod tests;
