//! MIDI output ring — a lock-free producer/consumer channel carrying
//! engine-produced MIDI out (e.g. the clock master's Beat Clock / MTC) from the
//! audio thread to an off-RT pump that forwards it to hardware output.

use std::sync::Arc;

use parking_lot::Mutex;
use ringbuf::{traits::*, HeapCons, HeapProd, HeapRb};
use tutti_midi_types::ump::MidiEvent;

const DEFAULT_CAPACITY: usize = 256;

pub struct MidiOutputProducer {
    producer: HeapProd<MidiEvent>,
}

impl MidiOutputProducer {
    /// Returns `false` if the ring buffer is full.
    #[inline]
    pub fn push(&mut self, event: MidiEvent) -> bool {
        self.producer.try_push(event).is_ok()
    }

    #[inline]
    pub fn push_slice(&mut self, events: &[MidiEvent]) -> usize {
        self.producer.push_slice(events)
    }
}

pub struct MidiOutputConsumer {
    consumer: HeapCons<MidiEvent>,
}

impl MidiOutputConsumer {
    pub fn drain_all(&mut self) -> Vec<MidiEvent> {
        let count = self.consumer.occupied_len();
        let mut events = Vec::with_capacity(count);
        while let Some(event) = self.consumer.try_pop() {
            events.push(event);
        }
        events
    }

    /// Drain up to `out.len()` events into a caller-owned slice, returning how
    /// many were written — the allocation-free counterpart to [`drain_all`], for
    /// the per-block hardware-output path. Events past `out.len()` stay queued
    /// for the next call.
    ///
    /// [`drain_all`]: Self::drain_all
    pub fn drain_into(&mut self, out: &mut [MidiEvent]) -> usize {
        let mut n = 0;
        while n < out.len() {
            match self.consumer.try_pop() {
                Some(event) => {
                    out[n] = event;
                    n += 1;
                }
                None => break,
            }
        }
        n
    }
}

/// A cheaply-clonable, `Send + Sync` push handle onto a MIDI output ring.
///
/// [`MidiOutputProducer`] is a single-producer `!Sync` ring half — fine for the
/// clock master (one owner on the audio thread), but the *track MIDI-out* path
/// needs many off-RT callers (an ECS send system, the UI, a clip player) to push
/// to the same ring. This wraps the producer in an `Arc<Mutex<…>>` so they can.
/// The lock is uncontended in practice (off-RT, brief) and never taken on the
/// audio thread — the consumer side stays lock-free.
#[derive(Clone)]
pub struct MidiOutHandle {
    producer: Arc<Mutex<MidiOutputProducer>>,
}

impl MidiOutHandle {
    /// Push one event; returns `false` if the ring is full (event dropped).
    #[inline]
    pub fn push(&self, event: MidiEvent) -> bool {
        self.producer.lock().push(event)
    }

    /// Push a batch; returns how many were accepted (fewer than `events.len()`
    /// only if the ring filled).
    #[inline]
    pub fn push_slice(&self, events: &[MidiEvent]) -> usize {
        self.producer.lock().push_slice(events)
    }
}

/// A shareable MIDI-out ring: a [`MidiOutHandle`] anyone can push into and the
/// [`MidiOutputConsumer`] the off-RT pump drains. Use for the track MIDI-out path
/// (vs. [`midi_output_channel`], whose bare producer suits a single owner).
pub fn shared_midi_output_channel(
    capacity: usize,
) -> (MidiOutHandle, MidiOutputConsumer) {
    let (producer, consumer) = midi_output_channel_with_capacity(capacity);
    (
        MidiOutHandle {
            producer: Arc::new(Mutex::new(producer)),
        },
        consumer,
    )
}

pub fn midi_output_channel() -> (MidiOutputProducer, MidiOutputConsumer) {
    midi_output_channel_with_capacity(DEFAULT_CAPACITY)
}

pub fn midi_output_channel_with_capacity(
    capacity: usize,
) -> (MidiOutputProducer, MidiOutputConsumer) {
    let rb = HeapRb::new(capacity);
    let (producer, consumer) = rb.split();
    (
        MidiOutputProducer { producer },
        MidiOutputConsumer { consumer },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note_on(channel: u8, note: u8) -> MidiEvent {
        MidiEvent::note_on(
            0,
            channel,
            note,
            tutti_midi_types::convert::midi1_velocity_to_midi2(100),
        )
    }

    fn note_off(channel: u8, note: u8) -> MidiEvent {
        MidiEvent::note_off(0, channel, note, 0)
    }

    #[test]
    fn test_channel_push_and_drain() {
        let (mut producer, mut consumer) = midi_output_channel();

        let event1 = note_on(0, 60);
        let event2 = note_off(0, 60).with_frame_offset(128);

        assert!(producer.push(event1));
        assert!(producer.push(event2));

        let events = consumer.drain_all();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].note(), Some(60));
        assert_eq!(events[1].note(), Some(60));
    }

    #[test]
    fn drain_into_is_alloc_free_and_partial() {
        let (mut prod, mut cons) = midi_output_channel();
        prod.push(note_on(0, 60));
        prod.push(note_on(0, 61));
        prod.push(note_on(0, 62));

        // Buffer holds 2 — two drained now, the third stays for the next call.
        let mut buf = [MidiEvent::noop(); 2];
        assert_eq!(cons.drain_into(&mut buf), 2);
        assert_eq!(buf[0].note(), Some(60));
        assert_eq!(buf[1].note(), Some(61));
        assert_eq!(cons.drain_into(&mut buf), 1);
        assert_eq!(buf[0].note(), Some(62));
        assert_eq!(cons.drain_into(&mut buf), 0);
    }

    #[test]
    fn test_capacity_overflow() {
        let (mut producer, _consumer) = midi_output_channel_with_capacity(4);

        let event = note_on(0, 60);

        assert!(producer.push(event));
        assert!(producer.push(event));
        assert!(producer.push(event));
        assert!(producer.push(event));

        assert!(!producer.push(event));
    }
}
