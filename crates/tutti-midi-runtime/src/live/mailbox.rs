//! [`MidiMailbox`]: a bounded, lock-free ring of MIDI events between one
//! thread and another, split into a [`MidiSender`] and a [`MidiReceiver`].
//!
//! It carries MIDI across a thread boundary the graph does not: into the
//! graph from a control thread ([`MidiQueueNode`](crate::MidiQueueNode), a
//! keyboard's notes), and, for a host's own outbound
//! path, out to a control thread. Inside
//! the graph MIDI travels on event ports, never through a mailbox.

use std::sync::Arc;
use tutti_graph::UmpAt;

use crossbeam_queue::ArrayQueue;
use tutti_ump::{Address, Message, MidiChannel, NoteNumber, Ump, Voice2};

use crate::traits::MidiOut;

/// The ring's default capacity, in events.
pub const MAILBOX_CAPACITY: usize = 256;

/// A bounded MIDI ring. Push and pop are lock-free and never allocate, so
/// either side may be the audio thread.
pub struct MidiMailbox {
    /// Each event with a raw `u32` stamp, not an `Offset`: the ring crosses
    /// threads ahead of any block, and what the stamp means is the reader's
    /// (the queue node clamps it into the block that plays it; a host's
    /// outbound path writes its own frame). The types stop here; a
    /// stamp becomes an `Offset` where a block is known.
    events: ArrayQueue<(u32, Ump)>,
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
    #[must_use]
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
/// Implements [`MidiOut`].
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
    pub fn queue(&self, events: &[UmpAt]) -> usize {
        let mut accepted = 0;
        for &(offset, ump) in events {
            if self.slot.events.push((offset.get(), ump)).is_err() {
                break;
            }
            accepted += 1;
        }
        accepted
    }

    /// [`queue`](Self::queue) for events stamped with a raw `u32` that is not
    /// an offset into a block (the out node's graph-clock frame, wrapped at
    /// 2^32). Same prefix semantics: the count accepted, never a gap.
    pub fn queue_stamped(&self, events: &[(u32, Ump)]) -> usize {
        let mut accepted = 0;
        for &event in events {
            if self.slot.events.push(event).is_err() {
                break;
            }
            accepted += 1;
        }
        accepted
    }

    /// Pushes a MIDI 2.0 Note On at `velocity` (16 bits) on group 1. A velocity
    /// of 0 is a Note On (M2-104 §7.4.2). Returns `false` if the ring was full.
    pub fn note_on(&self, channel: MidiChannel, note: NoteNumber, velocity: u16) -> bool {
        self.slot
            .events
            .push((0, voice(channel, Voice2::note_on(note, velocity))))
            .is_ok()
    }

    /// Pushes a MIDI 2.0 Note Off at release velocity 0 on group 1. Returns `false` if the ring was
    /// full (which would leave the note stuck).
    pub fn note_off(&self, channel: MidiChannel, note: NoteNumber) -> bool {
        self.slot
            .events
            .push((0, voice(channel, Voice2::note_off(note, 0))))
            .is_ok()
    }
}

impl MidiOut for MidiSender {
    fn queue(&self, events: &[UmpAt]) -> usize {
        self.queue(events)
    }
}

/// A group-1 channel voice message as a packet.
fn voice(channel: MidiChannel, v: Voice2) -> Ump {
    Message::Midi2(Address::first(channel), v).into()
}

/// The pop half of a [`MidiMailbox`]. One reader at a time: two polling the
/// same ring each take events the other never sees.
#[derive(Debug)]
pub struct MidiReceiver {
    slot: Arc<MidiMailbox>,
}

impl MidiReceiver {
    /// Pop up to `out.len()` events into `out`, oldest first, each with its raw
    /// stamp; return how many.
    pub fn poll_into(&self, out: &mut [(u32, Ump)]) -> usize {
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
    #[must_use]
    pub fn has_events(&self) -> bool {
        !self.slot.events.is_empty()
    }

    /// Drops everything waiting in the ring.
    pub fn clear(&self) {
        while self.slot.events.pop().is_some() {}
    }
}

#[cfg(test)]
mod tests;
