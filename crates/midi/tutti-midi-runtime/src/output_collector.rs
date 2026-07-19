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

    /// Uses `try_lock` to avoid blocking the audio thread.
    pub fn drain_all(&self) -> Vec<MidiEvent> {
        let mut consumers = match self.consumers.try_lock() {
            Some(guard) => guard,
            None => return Vec::new(),
        };
        let mut all_events = Vec::new();
        for consumer in consumers.iter_mut() {
            all_events.extend(consumer.drain_all());
        }
        all_events
    }

    /// Uses `try_lock` to avoid blocking the audio thread.
    pub fn has_pending(&self) -> bool {
        match self.consumers.try_lock() {
            Some(consumers) => consumers.iter().any(|c| c.has_pending()),
            None => false,
        }
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

        let events = aggregator.drain_all();
        assert_eq!(events.len(), 2);
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
