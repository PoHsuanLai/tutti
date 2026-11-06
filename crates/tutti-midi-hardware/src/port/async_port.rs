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

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use tutti_midi_types::ump::MidiEvent;
use tutti_types::AudioThreadCell;

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
/// let handle = inputs.open_input_port("In");
/// let second = handle.clone();
/// ```
///
/// and the manager hands one out only from
/// [`open_input_port`](super::HardwareMidiInputs::open_input_port).
pub struct InputProducerHandle {
    producer: HeapProd<(Instant, MidiEvent)>,
    port: PortId,
    /// Events [`push`](Self::push) found no room for, shared with the port.
    dropped: Arc<AtomicU64>,
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
    /// queued, and counted
    /// ([`HardwareMidiInputs::dropped_events`](super::HardwareMidiInputs::dropped_events)).
    /// Lock-free and allocation-free, so it is safe to call from a driver
    /// callback.
    #[inline]
    pub fn push(&mut self, event: MidiEvent, timestamp: Instant) -> bool {
        let taken = self.producer.try_push((timestamp, event)).is_ok();
        if !taken {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        taken
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
    /// Events the producer found the ring full for.
    dropped: Arc<AtomicU64>,
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
        let dropped = Arc::new(AtomicU64::new(0));
        (
            Self {
                id: port,
                name: name.into(),
                active: std::sync::atomic::AtomicBool::new(true),
                input: AudioThreadCell::new(consumer),
                dropped: Arc::clone(&dropped),
            },
            InputProducerHandle {
                producer,
                port,
                dropped,
            },
        )
    }

    /// How many events the producer dropped for want of room in the ring.
    pub(crate) fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
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
mod tests;
