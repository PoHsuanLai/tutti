//! One hardware input port: its ring's consumer, its active flag, and the
//! producer handle a driver callback pushes through.
//!
//! [`HardwareMidiInput`] is the per-connection half; [`HardwareMidiInputs`] in
//! `manager` holds N of them and drives the per-block drain across all of them.
//!
//! # Who owns which half of the ring
//!
//! Each port is one `ringbuf` `HeapRb`, split once at creation. The halves are
//! never stored together:
//!
//! - The **producer** half goes into the [`InputProducerHandle`] the creator is
//!   handed, and nowhere else. The handle owns it, is not `Clone`, and pushes
//!   through `&mut self`, so the borrow checker is what keeps a ring to one
//!   pusher: there is no second producer to reach.
//! - The **consumer** half stays in the port, in an [`AudioThreadCell`]: the
//!   audio thread's drain is its only reader (the control side never touches
//!   it), which is exactly that cell's "one borrow at a time" contract.
//!
//! Both halves hold the ring by `Arc`, so whichever is dropped last frees it,
//! and neither can outlive the memory it points at. A driver thread still
//! pushing after its port was retired pushes into a ring nobody drains, and
//! frees it when it stops; it can never write through a dangling pointer.
//!
//! [`HardwareMidiInputs`]: super::HardwareMidiInputs

use std::time::Instant;

use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use tutti_core::AudioThreadCell;
use tutti_midi_types::ump::MidiEvent;

use super::PortId;

/// The producer end of one port's input ring (timestamped events).
///
/// Owns the ring's only producer half. It is `Send`, so it moves to the thread
/// the OS delivers on, and **not** `Clone`: [`push`](Self::push) takes
/// `&mut self`, so a ring has exactly one pusher by construction. A driver
/// callback that is an `FnMut` (CoreMIDI's, ALSA's pump loop) owns its handle
/// outright.
///
/// Dropping the handle closes the producer side. The port's events already in
/// the ring stay there for the drain; the ring itself is freed by whichever of
/// the handle and the port goes last.
///
/// There is no second handle to a ring. It cannot be cloned:
///
/// ```compile_fail,E0599
/// let inputs = tutti_midi_hardware::HardwareMidiInputs::new(16);
/// let handle = inputs.create_input_port("In");
/// let second = handle.clone();
/// ```
///
/// and the manager hands one out only from
/// [`create_input_port`](super::HardwareMidiInputs::create_input_port).
pub struct InputProducerHandle {
    producer: HeapProd<(Instant, MidiEvent)>,
    port: PortId,
}

impl core::fmt::Debug for InputProducerHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("InputProducerHandle")
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl InputProducerHandle {
    /// Pushes one event with its arrival `timestamp`, which the drain converts
    /// into a `frame_offset` within the block that reads it.
    ///
    /// Returns `false` when the ring is full; the event is then dropped, not
    /// queued. Lock-free and allocation-free, so it is safe to call from a
    /// driver callback.
    #[inline]
    pub fn push(&mut self, event: MidiEvent, timestamp: Instant) -> bool {
        self.producer.try_push((timestamp, event)).is_ok()
    }

    /// The identity of the port this handle feeds: the key to pass to
    /// [`HardwareMidiInputs`](super::HardwareMidiInputs)' `set_port_active` or
    /// `remove_input_port`, and the tag the drain puts on its events.
    #[must_use]
    pub fn port_id(&self) -> PortId {
        self.port
    }
}

/// A hardware **input** port: the consumer end of a lock-free ring fed by the
/// driver callback thread (each event paired with its arrival `Instant`) and
/// drained by the engine cycle. This is the one MIDI path that genuinely needs
/// a cross-thread SPSC ring with wall-clock timestamps, which the
/// engine-internal [`MidiMailbox`](tutti_midi_runtime::MidiMailbox) mailbox
/// (the single MIDI-*out* ring) does not model. There is no output counterpart
/// here: outbound MIDI rides the mailbox to a
/// [`MidiOut`](tutti_midi_types::MidiOut) sink instead.
pub(crate) struct HardwareMidiInput {
    id: PortId,
    name: String,
    active: std::sync::atomic::AtomicBool,
    /// Reached only by the audio thread's drain, one borrow at a time.
    input: AudioThreadCell<HeapCons<(Instant, MidiEvent)>>,
}

impl HardwareMidiInput {
    /// A port named `name` whose ring holds `fifo_size` events, and the one
    /// producer handle for it, both named `port`.
    pub(crate) fn new(
        name: impl Into<String>,
        fifo_size: usize,
        port: PortId,
    ) -> (Self, InputProducerHandle) {
        let (producer, consumer) = HeapRb::new(fifo_size).split();
        (
            Self {
                id: port,
                name: name.into(),
                active: std::sync::atomic::AtomicBool::new(true),
                input: AudioThreadCell::new(consumer),
            },
            InputProducerHandle { producer, port },
        )
    }

    pub(crate) fn id(&self) -> PortId {
        self.id
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    #[inline]
    pub(crate) fn is_active(&self) -> bool {
        self.active.load(std::sync::atomic::Ordering::Acquire)
    }

    #[inline]
    pub(crate) fn set_active(&self, active: bool) {
        self.active
            .store(active, std::sync::atomic::Ordering::Release);
    }

    /// Pops at most `limit` events from this port's ring, passing each to `f`,
    /// and returns how many were taken.
    ///
    /// The unpopped remainder stays in the ring for the next block. `limit` is
    /// what keeps the caller's fixed-capacity sink from overflowing: the audio
    /// thread passes its remaining headroom. Audio thread only: this is the
    /// consumer's single reader.
    #[inline]
    pub(crate) fn drain_limited(
        &self,
        limit: usize,
        mut f: impl FnMut(Instant, MidiEvent),
    ) -> usize {
        let mut consumer = self.input.borrow_mut();
        let mut taken = 0;
        while taken < limit {
            match consumer.try_pop() {
                Some((timestamp, event)) => {
                    f(timestamp, event);
                    taken += 1;
                }
                None => break,
            }
        }
        taken
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
    use tutti_midi_types::{MidiChannel, MidiGroup};

    fn note_on(note: u8, vel: u8) -> MidiEvent {
        MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            note,
            midi1_velocity_to_midi2(vel),
        )
    }

    /// Test helper: drain input into a fresh Vec.
    fn read_input(port: &HardwareMidiInput) -> Vec<MidiEvent> {
        let mut buf = Vec::new();
        port.drain_limited(usize::MAX, |_, e| buf.push(e));
        buf
    }

    #[test]
    fn drain_stops_at_limit_and_keeps_the_rest() {
        let (port, mut handle) = HardwareMidiInput::new("Limited", 16, PortId::TEST);
        for i in 0..8 {
            assert!(handle.push(note_on(0x3C, 0x7F).with_frame_offset(i), Instant::now()));
        }

        let mut buf = Vec::new();
        let taken = port.drain_limited(3, |_, e| buf.push(e));
        assert_eq!(taken, 3, "drain must stop at the limit");
        assert_eq!(buf.len(), 3);

        // The undrained remainder is still queued, not lost.
        let rest = read_input(&port);
        assert_eq!(rest.len(), 5, "events past the limit stay in the ring");
    }

    #[test]
    fn drain_of_zero_takes_nothing() {
        let (port, mut handle) = HardwareMidiInput::new("Zero", 16, PortId::TEST);
        assert!(handle.push(note_on(0x3C, 0x7F), Instant::now()));

        let mut buf = Vec::new();
        assert_eq!(port.drain_limited(0, |_, e| buf.push(e)), 0);
        assert!(buf.is_empty());
        assert_eq!(read_input(&port).len(), 1, "a zero budget must not consume");
    }

    #[test]
    fn test_input_flow() {
        let (port, mut producer_handle) = HardwareMidiInput::new("Input", 256, PortId::TEST);

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
        let (_port, mut input_handle) = HardwareMidiInput::new("Full", 4, PortId::TEST);

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
        let (port, _handle) = HardwareMidiInput::new("ActiveTest", 256, PortId::TEST);
        assert!(port.is_active());

        port.set_active(false);
        assert!(!port.is_active());

        port.set_active(true);
        assert!(port.is_active());
    }

    /// A driver thread pushing while the drain pops: every event arrives, once,
    /// in order. Small enough to run under miri, which is what checks the two
    /// halves for a data race.
    ///
    /// Mutation (run): `drain_limited` discarding one extra pop after each
    /// event it hands on → every other event is lost → fails. The aliasing this test cannot express (two producers on one ring) is
    /// no longer writable: `push` takes `&mut self` and the handle is not
    /// `Clone`.
    #[test]
    fn a_driver_thread_and_the_drain_run_concurrently() {
        const EVENTS: u32 = 200;
        let (port, mut handle) = HardwareMidiInput::new("Concurrent", 8, PortId::TEST);
        let driver = std::thread::spawn(move || {
            for i in 0..EVENTS {
                let event = MidiEvent::noop().with_frame_offset(i);
                while !handle.push(event, Instant::now()) {
                    std::thread::yield_now();
                }
            }
        });

        // Drain until the driver is done and a last pass finds nothing, so a
        // lost event fails the comparison below rather than hanging the loop.
        let mut seen = Vec::new();
        loop {
            let done = driver.is_finished();
            let taken = port.drain_limited(usize::MAX, |_, e| seen.push(e.frame_offset));
            if done && taken == 0 {
                break;
            }
            std::thread::yield_now();
        }
        driver.join().expect("driver thread");
        assert_eq!(seen, (0..EVENTS).collect::<Vec<_>>());
    }
}
