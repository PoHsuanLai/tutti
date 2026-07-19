use std::time::Instant;

use tutti_midi_types::ump::MidiEvent;

use super::spsc::{SpscProducer, SpscRing};

/// Producer handle for a port's input ring (timestamped events).
///
/// # Safety
/// Must only be used from a single thread (the midir callback thread) — the
/// SPSC single-producer invariant. The wrapped [`SpscProducer`] encapsulates
/// the unsafe; this newtype only pairs each event with its arrival `Instant`.
#[derive(Clone)]
pub struct InputProducerHandle {
    producer: SpscProducer<(Instant, MidiEvent)>,
}

impl InputProducerHandle {
    #[inline]
    pub fn push(&self, event: MidiEvent, timestamp: Instant) -> bool {
        self.producer.push((timestamp, event))
    }
}

/// Producer handle for a port's output ring.
///
/// # Safety
/// Must only be used from a single thread (the audio thread) — the SPSC
/// single-producer invariant.
#[derive(Clone)]
pub struct OutputProducerHandle {
    producer: SpscProducer<MidiEvent>,
}

impl OutputProducerHandle {
    #[inline]
    pub fn push(&self, event: MidiEvent) -> bool {
        self.producer.push(event)
    }
}

pub struct AsyncMidiPort {
    name: String,
    active: std::sync::atomic::AtomicBool,
    input: SpscRing<(Instant, MidiEvent)>,
    output: SpscRing<MidiEvent>,
}

impl AsyncMidiPort {
    pub fn new(name: impl Into<String>, fifo_size: usize) -> Self {
        Self {
            name: name.into(),
            active: std::sync::atomic::AtomicBool::new(true),
            input: SpscRing::new(fifo_size),
            output: SpscRing::new(fifo_size),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    #[inline]
    pub fn is_active(&self) -> bool {
        self.active.load(std::sync::atomic::Ordering::Acquire)
    }

    #[inline]
    pub fn set_active(&self, active: bool) {
        self.active
            .store(active, std::sync::atomic::Ordering::Release);
    }

    pub fn input_producer_handle(&self) -> InputProducerHandle {
        InputProducerHandle {
            producer: self.input.producer(),
        }
    }

    pub fn output_producer_handle(&self) -> OutputProducerHandle {
        OutputProducerHandle {
            producer: self.output.producer(),
        }
    }

    /// Drain this port's input ring into `sink`, tagging each event with
    /// `port_index`. Generic over the sink (`Vec`, `SmallVec`, …) so callers
    /// can use whatever RT buffer they hold.
    #[inline]
    pub fn cycle_start_read_input_into(
        &self,
        sink: &mut impl Extend<(Instant, usize, MidiEvent)>,
        port_index: usize,
    ) {
        self.input
            .drain_each(|(timestamp, event)| sink.extend(core::iter::once((timestamp, port_index, event))));
    }

    /// Drain this port's output ring into `sink`, tagging each event with
    /// `port_index`. Generic over the sink as above.
    #[inline]
    pub fn cycle_end_flush_output_into(
        &self,
        sink: &mut impl Extend<(usize, MidiEvent)>,
        port_index: usize,
    ) {
        self.output
            .drain_each(|event| sink.extend(core::iter::once((port_index, event))));
    }
}

impl core::fmt::Debug for AsyncMidiPort {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AsyncMidiPort")
            .field("name", &self.name)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_midi_types::convert::midi1_velocity_to_midi2;

    fn note_on(note: u8, vel: u8) -> MidiEvent {
        MidiEvent::note_on(0, 0, note, midi1_velocity_to_midi2(vel))
    }

    fn note_off(note: u8) -> MidiEvent {
        MidiEvent::note_off(0, 0, note, 0)
    }

    /// Test helper: drain input into a fresh Vec.
    fn read_input(port: &AsyncMidiPort) -> Vec<MidiEvent> {
        let mut buf = Vec::new();
        port.cycle_start_read_input_into(&mut buf, 0);
        buf.into_iter().map(|(_, _, e)| e).collect()
    }

    /// Test helper: drain output into a fresh Vec.
    fn flush_output(port: &AsyncMidiPort) -> Vec<MidiEvent> {
        let mut buf = Vec::new();
        port.cycle_end_flush_output_into(&mut buf, 0);
        buf.into_iter().map(|(_, e)| e).collect()
    }

    #[test]
    fn test_input_flow() {
        let port = AsyncMidiPort::new("Input", 256);
        let producer_handle = port.input_producer_handle();

        let event = note_on(0x3C, 0x7F);
        assert!(producer_handle.push(event, Instant::now()));

        let events = read_input(&port);
        assert_eq!(events.len(), 1);
        assert!(events[0].is_note_on());
        assert_eq!(events[0].note(), Some(0x3C));
        assert_eq!(events[0].velocity_u7(), Some(0x7F));
    }

    #[test]
    fn test_output_flow() {
        let port = AsyncMidiPort::new("Output", 256);
        let output_handle = port.output_producer_handle();

        let event = note_off(0x3C);
        assert!(output_handle.push(event));

        let events = flush_output(&port);
        assert_eq!(events.len(), 1);
        assert!(events[0].is_note_off());
        assert_eq!(events[0].note(), Some(0x3C));
    }

    #[test]
    fn test_fifo_full() {
        let port = AsyncMidiPort::new("Full", 4);
        let output_handle = port.output_producer_handle();

        for i in 0..4 {
            let event = note_on(0x3C, 0x7F).with_frame_offset(i);
            assert!(output_handle.push(event), "Failed to write event {}", i);
        }

        let event = note_on(0x3C, 0x7F);
        assert!(!output_handle.push(event), "FIFO should be full");
    }

    #[test]
    fn test_active_flag_toggle() {
        let port = AsyncMidiPort::new("ActiveTest", 256);
        assert!(port.is_active());

        port.set_active(false);
        assert!(!port.is_active());

        port.set_active(true);
        assert!(port.is_active());
    }

    #[test]
    fn test_input_output_isolation() {
        let port = AsyncMidiPort::new("Isolation", 256);
        let input_handle = port.input_producer_handle();
        let output_handle = port.output_producer_handle();

        input_handle.push(note_on(60, 100), Instant::now());

        assert!(
            flush_output(&port).is_empty(),
            "Output should not receive input events"
        );
        assert_eq!(read_input(&port).len(), 1);

        output_handle.push(note_off(60));

        assert!(
            read_input(&port).is_empty(),
            "Input should not receive output events"
        );
        assert_eq!(flush_output(&port).len(), 1);
    }
}
