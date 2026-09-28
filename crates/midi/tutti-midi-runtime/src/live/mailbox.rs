//! [`MidiMailbox`]: a bounded, lock-free ring of MIDI events between one
//! thread and another, split into a [`MidiSender`] and a [`MidiReceiver`].
//!
//! It carries MIDI across a thread boundary the graph does not: into the
//! graph from a control thread ([`MidiQueueNode`](crate::MidiQueueNode), a
//! keyboard's notes), and out of it to a control thread
//! ([`MidiOutNode`](crate::MidiOutNode), what a hardware pump sends). Inside
//! the graph MIDI travels on event ports, never through a mailbox.

use std::sync::Arc;

use crossbeam_queue::ArrayQueue;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiChannel, MidiGroup};

/// The ring's default capacity, in events.
pub const MAILBOX_CAPACITY: usize = 256;

/// A bounded MIDI ring. Push and pop are lock-free and never allocate, so
/// either side may be the audio thread.
pub struct MidiMailbox {
    events: ArrayQueue<MidiEvent>,
}

impl std::fmt::Debug for MidiMailbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiMailbox")
            .field("events", &self.events.len())
            .finish()
    }
}

impl MidiMailbox {
    /// A ring of [`MAILBOX_CAPACITY`] events, as a sender and a receiver.
    pub fn pair() -> (MidiSender, MidiReceiver) {
        Self::with_capacity(MAILBOX_CAPACITY)
    }

    /// A ring of `capacity` events (at least one), as a sender and a
    /// receiver.
    pub fn with_capacity(capacity: usize) -> (MidiSender, MidiReceiver) {
        let slot = Arc::new(Self {
            events: ArrayQueue::new(capacity.max(1)),
        });
        (
            MidiSender {
                slot: Arc::clone(&slot),
            },
            MidiReceiver { slot },
        )
    }
}

/// The push half of a [`MidiMailbox`]. Cheap to clone; every clone pushes into
/// the same ring.
///
/// Implements [`tutti_midi_types::MidiOut`].
#[derive(Clone, Debug)]
pub struct MidiSender {
    slot: Arc<MidiMailbox>,
}

impl MidiSender {
    /// Pushes `events` and returns how many were accepted. `< events.len()` means
    /// the ring was full and the rest were dropped (a prefix is accepted, so
    /// the stream stays in order). Dropping a note-off whose note-on landed is
    /// what leaves a note stuck, so a caller with anywhere to report it must.
    /// Never blocks.
    pub fn queue(&self, events: &[MidiEvent]) -> usize {
        let mut accepted = 0;
        for &event in events {
            if self.slot.events.push(event).is_err() {
                break;
            }
            accepted += 1;
        }
        accepted
    }

    /// Pushes a MIDI 1.0 note-on (7-bit velocity) on group 1. Returns `false`
    /// if the ring was full.
    pub fn note_on(&self, channel: MidiChannel, note: u8, velocity: u8) -> bool {
        self.slot
            .events
            .push(MidiEvent::note_on_7bit(
                MidiGroup::FIRST,
                channel,
                note,
                velocity,
            ))
            .is_ok()
    }

    /// Pushes a MIDI 1.0 note-off on group 1. Returns `false` if the ring was
    /// full (which would leave the note stuck).
    pub fn note_off(&self, channel: MidiChannel, note: u8) -> bool {
        self.slot
            .events
            .push(MidiEvent::note_off(MidiGroup::FIRST, channel, note, 0))
            .is_ok()
    }
}

impl tutti_midi_types::MidiOut for MidiSender {
    fn queue(&self, events: &[MidiEvent]) -> usize {
        self.queue(events)
    }
}

/// The pop half of a [`MidiMailbox`]. One reader at a time: two polling the
/// same ring each take events the other never sees.
#[derive(Debug)]
pub struct MidiReceiver {
    slot: Arc<MidiMailbox>,
}

impl MidiReceiver {
    /// Pop up to `out.len()` events into `out`, oldest first; return how many.
    pub fn poll_into(&self, out: &mut [MidiEvent]) -> usize {
        let mut count = 0;
        for slot in out.iter_mut() {
            match self.slot.events.pop() {
                Some(event) => {
                    *slot = event;
                    count += 1;
                }
                None => break,
            }
        }
        count
    }

    /// Whether anything is waiting.
    pub fn has_events(&self) -> bool {
        !self.slot.events.is_empty()
    }

    /// Drops everything waiting in the ring.
    pub fn clear(&self) {
        while self.slot.events.pop().is_some() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note_on(note: u8, vel_u7: u8) -> MidiEvent {
        MidiEvent::note_on_7bit(MidiGroup::FIRST, MidiChannel::FIRST, note, vel_u7)
    }

    fn note_off(note: u8) -> MidiEvent {
        MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, note, 0)
    }

    #[test]
    fn sender_pushes_to_receiver() {
        let (sender, receiver) = MidiMailbox::pair();
        sender.queue(&[note_on(60, 100), note_off(60)]);
        assert!(receiver.has_events());
        let mut buf = [MidiEvent::noop(); 16];
        assert_eq!(receiver.poll_into(&mut buf), 2);
        assert_eq!(buf[..2], [note_on(60, 100), note_off(60)]);
        assert!(!receiver.has_events());
    }

    #[test]
    fn cloned_sender_pushes_to_same_receiver() {
        let (sender, receiver) = MidiMailbox::pair();
        let s2 = sender.clone();
        sender.queue(&[note_on(60, 100)]);
        s2.queue(&[note_on(64, 100)]);
        let mut buf = [MidiEvent::noop(); 16];
        assert_eq!(receiver.poll_into(&mut buf), 2);
    }

    /// The `note_on` / `note_off` conveniences build the same events the
    /// explicit constructors do.
    ///
    /// Mutation: a helper emitting another channel or velocity scaling → fails.
    #[test]
    fn sender_note_helpers_match_explicit_events() {
        let (sender, receiver) = MidiMailbox::pair();
        sender.note_on(MidiChannel::FIRST, 60, 100);
        sender.note_off(MidiChannel::FIRST, 60);
        let mut buf = [MidiEvent::noop(); 4];
        assert_eq!(receiver.poll_into(&mut buf), 2);
        assert_eq!(buf[0], note_on(60, 100));
        assert_eq!(buf[1], note_off(60));
    }

    /// A push into a ring whose receiver is gone still lands (the count says
    /// so), rather than silently reporting zero.
    #[test]
    fn dropped_receiver_keeps_sender_pushable() {
        let (sender, receiver) = MidiMailbox::pair();
        drop(receiver);
        assert_eq!(sender.queue(&[note_on(60, 100), note_on(64, 100)]), 2);
        assert!(sender.note_on(MidiChannel::FIRST, 67, 100));
    }

    /// A partial accept reports the prefix that landed, not the count offered.
    ///
    /// Mutation: `queue` returning `events.len()` → fails.
    #[test]
    fn a_partial_accept_reports_how_many_landed() {
        let (sender, _receiver) = MidiMailbox::with_capacity(5);
        assert_eq!(sender.queue(&[note_on(1, 1), note_on(2, 1)]), 2);
        let batch = [
            note_on(60, 100),
            note_on(61, 100),
            note_on(62, 100),
            note_on(63, 100),
        ];
        assert_eq!(sender.queue(&batch), 3);
    }
}
