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

use tutti_graph::{EventCapacity, EventInletWriter, Losses, Refused, Stamp};
use tutti_ump::{Address, Message, MidiChannel, MidiGroup, NoteNumber, NoteVelocity, Voice2};

/// The ring a live input's inlet is opened with, in events (it is also the
/// waiting list's size and the port's declared capacity).
pub const LIVE_INLET_CAPACITY: EventCapacity = EventCapacity::of(256);

/// An [`EventInletWriter`] with the key helpers. One producer, as the writer
/// is: share it behind a lock on the control side if it must be `Clone`.
///
/// **Keys are a performance, not part of a bounce.** What they send plays in
/// the live graph and reaches the device output, but an event inlet's fork is
/// silent, so a bounce renders the instrument with none of it. Notes an export
/// must hold are a clip on the timeline (`MidiClipNode`, through the
/// session's `play_clip`).
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

    /// What the inlet has lost, in events (the writer's
    /// [`counts`](EventInletWriter::counts), read).
    #[must_use]
    pub fn losses(&self) -> Losses {
        self.writer.counts().losses()
    }

    /// The Group every key helper sends on.
    #[must_use]
    pub fn group(&self) -> MidiGroup {
        self.group
    }

    /// A MIDI 2.0 Note On at `velocity` on the keys' Group, for `at`.
    ///
    /// The velocity is the 16-bit field of M2-104 §7.4.2, `0` to `0xFFFF`, and
    /// not a MIDI 1.0 velocity: `NoteVelocity::new(100)` is nearly silent. A
    /// velocity of 0 is a Note On.
    ///
    /// # Errors
    ///
    /// As [`EventInletWriter::push`].
    ///
    /// # Examples
    ///
    /// ```
    /// use tutti_graph::{EventCapacity, EventInletNode, Stamp};
    /// use tutti_midi_runtime::MidiKeys;
    /// use tutti_ump::{MidiChannel, MidiGroup, NoteNumber, NoteVelocity};
    ///
    /// let (_node, writer) = EventInletNode::pair(EventCapacity::of(16));
    /// let mut keys = MidiKeys::new(writer, MidiGroup::FIRST);
    /// // Three quarters of the 16-bit range.
    /// keys.note_on(
    ///     Stamp::NextBlock,
    ///     MidiChannel::FIRST,
    ///     NoteNumber::of(60),
    ///     NoteVelocity::new(0xC000),
    /// )
    /// .unwrap();
    /// ```
    pub fn note_on(
        &mut self,
        at: impl Into<Stamp>,
        channel: MidiChannel,
        note: NoteNumber,
        velocity: NoteVelocity,
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
        self.voice(at, channel, Voice2::note_off(note, NoteVelocity::MIN))
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
        self.writer.push(at, Message::Midi2(address, voice))
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
