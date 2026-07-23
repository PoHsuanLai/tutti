use std::time::Instant;

use tutti_midi_types::ump::MidiEvent;

use super::spsc::{SpscProducer, SpscRing};

/// Producer handle for a port's input ring (timestamped events).
///
/// # Safety
/// Must only be used from a single thread (the midir callback thread) — the
/// SPSC single-producer invariant. The wrapped `SpscProducer` encapsulates
/// the unsafe; this newtype only pairs each event with its arrival `Instant`.
#[derive(Clone)]
pub struct InputProducerHandle {
    producer: SpscProducer<(Instant, MidiEvent)>,
}

impl core::fmt::Debug for InputProducerHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The inner SPSC producer is a raw-pointer handle with no inspectable
        // state; just name the type.
        f.debug_struct("InputProducerHandle")
            .finish_non_exhaustive()
    }
}

impl InputProducerHandle {
    #[inline]
    pub fn push(&self, event: MidiEvent, timestamp: Instant) -> bool {
        self.producer.push((timestamp, event))
    }
}

/// A hardware **input** port: a lock-free ring fed by the midir callback thread
/// (each event paired with its arrival `Instant`) and drained by the engine
/// cycle. This is the one MIDI path that genuinely needs [`SpscRing`] — a
/// cross-thread producer/consumer with wall-clock timestamps — which the
/// engine-internal [`MidiMailbox`](tutti_midi_runtime::MidiMailbox) mailbox
/// (the single MIDI-*out* ring) does not model. There is no output counterpart
/// here: outbound MIDI rides the mailbox → [`OutputThread`](crate) sink instead.
pub struct HardwareMidiInput {
    name: String,
    active: std::sync::atomic::AtomicBool,
    input: SpscRing<(Instant, MidiEvent)>,
}

impl HardwareMidiInput {
    pub fn new(name: impl Into<String>, fifo_size: usize) -> Self {
        Self {
            name: name.into(),
            active: std::sync::atomic::AtomicBool::new(true),
            input: SpscRing::new(fifo_size),
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

    /// Drain this port's input ring into `sink`, tagging each event with
    /// `port_index`. Generic over the sink (`Vec`, `SmallVec`, …) so callers
    /// can use whatever RT buffer they hold.
    #[inline]
    pub fn cycle_start_read_input_into(
        &self,
        sink: &mut impl Extend<(Instant, usize, MidiEvent)>,
        port_index: usize,
    ) {
        self.input.drain_each(|(timestamp, event)| {
            sink.extend(core::iter::once((timestamp, port_index, event)))
        });
    }
}

impl core::fmt::Debug for HardwareMidiInput {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HardwareMidiInput")
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

    /// Test helper: drain input into a fresh Vec.
    fn read_input(port: &HardwareMidiInput) -> Vec<MidiEvent> {
        let mut buf = Vec::new();
        port.cycle_start_read_input_into(&mut buf, 0);
        buf.into_iter().map(|(_, _, e)| e).collect()
    }

    #[test]
    fn test_input_flow() {
        let port = HardwareMidiInput::new("Input", 256);
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
    fn test_fifo_full() {
        let port = HardwareMidiInput::new("Full", 4);
        let input_handle = port.input_producer_handle();

        for i in 0..4 {
            let event = note_on(0x3C, 0x7F).with_frame_offset(i);
            assert!(
                input_handle.push(event, Instant::now()),
                "Failed to write event {}",
                i
            );
        }

        let event = note_on(0x3C, 0x7F);
        assert!(
            !input_handle.push(event, Instant::now()),
            "FIFO should be full"
        );
    }

    #[test]
    fn test_active_flag_toggle() {
        let port = HardwareMidiInput::new("ActiveTest", 256);
        assert!(port.is_active());

        port.set_active(false);
        assert!(!port.is_active());

        port.set_active(true);
        assert!(port.is_active());
    }
}
