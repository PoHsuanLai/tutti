//! One hardware input port: the producer handle a driver callback pushes
//! through, and the port's entry in the list.
//!
//! [`HardwareMidiInput`] is the entry [`HardwareMidiInputs`] keeps;
//! the [`InputProducerHandle`] is the only way events enter.
//!
//! # Who owns what
//!
//! The handle owns the device's end of its inlet
//! ([`DeviceInput`]), is not `Clone`, and
//! pushes through `&mut self`: the borrow checker keeps an inlet to one
//! pusher. The entry holds a view of the inlet's counts and the active flag
//! the handle reads, so the control side reads and sets them without touching
//! the thread that pushes. A driver thread still pushing after its port was
//! retired pushes into an inlet nothing reads.
//!
//! [`HardwareMidiInputs`]: super::HardwareMidiInputs

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use tutti_midi_runtime::{DeviceInput, DeviceSlot, InletCounts};
use tutti_ump::Ump;

use super::PortId;

/// The producer end of one port: the driver thread's. Owns the inlet's only
/// writer. It is `Send`, so it moves to the thread the OS delivers on, and
/// **not** `Clone`: [`push`](Self::push) takes `&mut self`, so an inlet has
/// exactly one pusher by construction. A driver callback that is an `FnMut`
/// (CoreMIDI's, ALSA's pump loop) owns its handle outright.
///
/// There is no second handle to a port. It cannot be cloned:
///
/// ```compile_fail,E0599
/// fn second(handle: tutti_midi_hardware::InputProducerHandle) {
///     let _ = handle.clone();
/// }
/// ```
///
/// and the list hands one out only from
/// [`open_input_port`](super::HardwareMidiInputs::open_input_port).
pub struct InputProducerHandle {
    input: DeviceInput,
    port: PortId,
    /// Whether the port delivers: shared with the port's entry.
    active: Arc<AtomicBool>,
    /// Where the device's UMP Stream messages go: not into the graph.
    stream: HeapProd<Ump>,
    /// How many of them the ring had no room for: shared with the port's entry.
    stream_lost: Arc<AtomicU64>,
}

/// How many UMP Stream messages a port holds between two looks of the control
/// side. A discovery's replies are a few dozen packets (names are up to 98 and
/// 91 bytes, 14 and 13 to a packet, for up to 32 blocks); the control side
/// reads them at every update, and a reply lost to a full ring is asked for
/// again when its step times out.
pub(crate) const STREAM_CAPACITY: usize = 512;

impl core::fmt::Debug for InputProducerHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("InputProducerHandle")
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl InputProducerHandle {
    /// Pushes one event the OS delivered at `timestamp`: it is placed on the
    /// frame the engine's clock puts that time on, plus the inlets' lead
    /// ([`DeviceInlets`](tutti_midi_runtime::DeviceInlets)), so the order and
    /// spacing of the device's events are kept to the clock map's accuracy.
    ///
    /// A **UMP Stream message** (type `0xF`, M2-104 §7.1) is the device's talk
    /// about itself, not music: it never reaches the graph. It goes to the
    /// port's stream ring, which the control side reads
    /// ([`HardwareMidiInputs::drain_stream`](super::HardwareMidiInputs::drain_stream)),
    /// and one the ring has no room for is counted
    /// ([`PortInfo::stream_lost`](super::PortInfo::stream_lost)) and returns
    /// `true`: it was not lost from the inlet.
    ///
    /// A **JR Clock or JR Timestamp** (type `0x0`, M2-104 §7.2.2) is dropped
    /// here too, and returns `true`. The session asks for JR Timestamps off, and
    /// a device that sends them anyway has them ignored: the placement of an
    /// event is its OS arrival stamp, and §7.2.2.1 leaves what a Receiver does
    /// with JR to it ("the Receiver may use smoothing or averaging of time from
    /// each JR Clock message ... to determine a steady JR Clock to render
    /// against"). Reading them would need a `JrReceiver` per device, its delay
    /// applied to the arrival stamp, and a clock to reconstruct, to refine a
    /// time the OS has already stamped; and nothing downstream reads a
    /// timestamp utility message, so one that reached the graph would be noise.
    ///
    /// Returns `false` when the device's inlet is full; the event is then
    /// dropped, not queued, and counted
    /// ([`HardwareMidiInputs::dropped_events`](super::HardwareMidiInputs::dropped_events)).
    /// An **inactive** port ([`set_port_active`](super::HardwareMidiInputs::set_port_active))
    /// discards the event and returns `true`: it was not lost for want of room.
    /// Lock-free and allocation-free, so it is safe to call from a driver
    /// callback.
    #[inline]
    pub fn push(&mut self, event: Ump, timestamp: Instant) -> bool {
        if !self.active.load(Ordering::Acquire) {
            return true;
        }
        if event.is_stream() {
            if self.stream.try_push(event).is_err() {
                self.stream_lost.fetch_add(1, Ordering::Relaxed);
            }
            return true;
        }
        if event.is_jr() {
            return true;
        }
        self.input.push(event, timestamp)
    }

    /// The identity of the port this handle feeds: the key to pass to
    /// [`HardwareMidiInputs`](super::HardwareMidiInputs)' `set_port_active` or
    /// `remove_input_port`.
    #[must_use]
    pub fn port_id(&self) -> PortId {
        self.port
    }
}

/// A hardware **input** port in the list: its name, whether it delivers, and
/// the counts of its inlet. There is no output counterpart here: outbound MIDI
/// rides an event tap to a [`MidiOut`](tutti_midi_runtime::MidiOut) sink.
pub(crate) struct HardwareMidiInput {
    id: PortId,
    name: String,
    active: Arc<AtomicBool>,
    counts: InletCounts,
    slot: DeviceSlot,
    /// The reading end of the stream ring; control side, one reader at a time.
    stream: Mutex<HeapCons<Ump>>,
    stream_lost: Arc<AtomicU64>,
}

impl HardwareMidiInput {
    /// A port named `name` over `input`, and the producer handle for it, both
    /// named `port`.
    pub(crate) fn new(
        name: impl Into<String>,
        input: DeviceInput,
        port: PortId,
    ) -> (Self, InputProducerHandle) {
        let active = Arc::new(AtomicBool::new(true));
        let stream_lost = Arc::new(AtomicU64::new(0));
        let (stream_in, stream_out) = HeapRb::<Ump>::new(STREAM_CAPACITY).split();
        (
            Self {
                id: port,
                name: name.into(),
                active: Arc::clone(&active),
                counts: input.counts(),
                slot: input.slot(),
                stream: Mutex::new(stream_out),
                stream_lost: Arc::clone(&stream_lost),
            },
            InputProducerHandle {
                input,
                port,
                active,
                stream: stream_in,
                stream_lost,
            },
        )
    }

    /// How many events the inlet had no room for.
    pub(crate) fn dropped(&self) -> u64 {
        self.counts.lost()
    }

    /// How many events the graph found already past.
    pub(crate) fn late(&self) -> u64 {
        self.counts.late()
    }

    /// How many Stream messages the ring had no room for.
    pub(crate) fn stream_lost(&self) -> u64 {
        self.stream_lost.load(Ordering::Relaxed)
    }

    /// Hands every Stream message held to `take`, oldest first.
    pub(crate) fn drain_stream(&self, mut take: impl FnMut(Ump)) {
        let mut ring = self.stream.lock().unwrap_or_else(PoisonError::into_inner);
        while let Some(ump) = ring.try_pop() {
            take(ump);
        }
    }

    pub(crate) fn id(&self) -> PortId {
        self.id
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn slot(&self) -> DeviceSlot {
        self.slot
    }

    #[inline]
    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    #[inline]
    pub(crate) fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Release);
    }
}

impl core::fmt::Debug for HardwareMidiInput {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HardwareMidiInput")
            .field("name", &self.name)
            .finish()
    }
}
