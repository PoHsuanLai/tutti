//! MIDI output collection — ring-buffer channels for collecting MIDI output
//! from multiple audio units, aggregated for hardware output.

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
    #[inline]
    pub fn pop(&mut self) -> Option<MidiEvent> {
        self.consumer.try_pop()
    }

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

    #[inline]
    pub fn has_pending(&self) -> bool {
        !self.consumer.is_empty()
    }

    #[inline]
    pub fn pending_count(&self) -> usize {
        self.consumer.occupied_len()
    }
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

pub struct MidiOutputAggregator {
    consumers: Mutex<Vec<MidiOutputConsumer>>,
}

impl MidiOutputAggregator {
    pub fn new() -> Self {
        Self {
            consumers: Mutex::new(Vec::new()),
        }
    }

    pub fn add_consumer(&self, consumer: MidiOutputConsumer) {
        self.consumers.lock().push(consumer);
    }

    /// Drain every consumer, or `None` if the consumer list was momentarily
    /// locked (add/remove in flight). Uses `try_lock` to avoid blocking the audio
    /// thread — and returns `None` rather than an empty `Vec` on contention so a
    /// caller can tell "lock busy, try again" apart from "genuinely nothing
    /// pending" (an empty `Vec` means the latter). A shutdown-drain loop should
    /// treat `None` as "retry", not "done".
    pub fn drain_all(&self) -> Option<Vec<MidiEvent>> {
        let mut consumers = self.consumers.try_lock()?;
        let mut all_events = Vec::new();
        for consumer in consumers.iter_mut() {
            all_events.extend(consumer.drain_all());
        }
        Some(all_events)
    }

    /// Whether any consumer has pending events, or `None` if the list was
    /// momentarily locked (same busy-vs-empty distinction as [`drain_all`]).
    ///
    /// [`drain_all`]: Self::drain_all
    pub fn has_pending(&self) -> Option<bool> {
        let consumers = self.consumers.try_lock()?;
        Some(consumers.iter().any(|c| c.has_pending()))
    }
}

impl Default for MidiOutputAggregator {
    fn default() -> Self {
        Self::new()
    }
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
    fn test_aggregator() {
        let aggregator = MidiOutputAggregator::new();

        let (mut prod1, cons1) = midi_output_channel();
        let (mut prod2, cons2) = midi_output_channel();

        aggregator.add_consumer(cons1);
        aggregator.add_consumer(cons2);

        prod1.push(note_on(0, 60));
        prod2.push(note_on(1, 72));

        // Lock is free here, so drain_all yields Some; the two events are present.
        let events = aggregator.drain_all().expect("lock free");
        assert_eq!(events.len(), 2);
        // Now empty (but not busy) → Some(empty), distinct from None.
        assert_eq!(aggregator.drain_all(), Some(Vec::new()));
        assert_eq!(aggregator.has_pending(), Some(false));
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
