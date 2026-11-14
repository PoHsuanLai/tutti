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
use tutti_graph::Losses;
use tutti_midi_runtime::{DeviceInput, DeviceSlot, InletCounts};
use tutti_ump::Ump;

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
    /// Whether the port delivers: shared with the port's entry.
    active: Arc<AtomicBool>,
    /// Where the device's UMP Stream messages go: not into the graph.
    stream: HeapProd<Ump>,
    /// How many of them the ring had no room for: shared with the port's entry.
    stream_lost: Arc<AtomicU64>,
    /// Whether the control side wants a copy of the SysEx7 packets the device
    /// sends (it is waiting for an Identity Reply): shared with the port's entry.
    hear_sysex: Arc<AtomicBool>,
}

/// How many UMP Stream messages a port holds between two looks of the control
/// side. A discovery's replies are a few dozen packets (names are up to 98 and
/// 91 bytes, 14 and 13 to a packet, for up to 32 blocks); the control side
/// reads them at every update, and a reply lost to a full ring is asked for
/// again when its step times out.
pub(crate) const STREAM_CAPACITY: usize = 512;

/// The UMP message type of a 7-bit System Exclusive packet (M2-104 §7.7).
const SYSEX7: u8 = 0x3;

impl core::fmt::Debug for InputProducerHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("InputProducerHandle")
            .field("slot", &self.input.slot())
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
    /// (`HardwareMidiInputs::drain_stream`),
    /// and one the ring has no room for is counted
    /// ([`PortInfo::stream_lost`](super::PortInfo::stream_lost)) and is
    /// `Diverted` all the same: it was not lost from the inlet.
    ///
    /// A **SysEx7 packet** (type `0x3`) is music's too, and goes to the graph
    /// whatever else happens to it; but while the control side is waiting for a
    /// device to say what it is (`HardwareMidiInputs::hear_sysex`,
    /// an Identity Request is out, or MIDI-CI runs), a copy goes to the same stream ring. It is a
    /// copy, not a diversion, because the packets of a message are not
    /// recognisable one by one: whether a SysEx is an Identity Reply is known
    /// only once it is whole, and a patch dump that arrives meanwhile must still
    /// reach the graph.
    ///
    /// A **JR Clock or JR Timestamp** (type `0x0`, M2-104 §7.2.2) is dropped
    /// here too, and is `Diverted`. The session asks for JR Timestamps off, and
    /// a device that sends them anyway has them ignored: the placement of an
    /// event is its OS arrival stamp, and §7.2.2.1 leaves what a Receiver does
    /// with JR to it (it may smooth or average the JR Clock messages). Reading them would need a `JrReceiver` per device, its delay
    /// applied to the arrival stamp, and a clock to reconstruct, to refine a
    /// time the OS has already stamped; and nothing downstream reads a
    /// timestamp utility message, so one that reached the graph would be noise.
    ///
    /// Returns what became of it: [`Pushed::Sent`] into the graph's inlet,
    /// [`Pushed::Diverted`] (not music, or the port is paused, so nothing was
    /// lost for want of room), or [`Pushed::Full`] when the device's inlet is
    /// full: the event is then dropped, not queued, and counted
    /// ([`HardwareMidiInputs::losses`](super::HardwareMidiInputs::losses)).
    /// An **inactive** port ([`set_port_active`](super::HardwareMidiInputs::set_port_active))
    /// discards the event and is `Diverted`. Lock-free and allocation-free, so
    /// it is safe to call from a driver callback.
    #[inline]
    pub fn push(&mut self, event: impl Into<Ump>, timestamp: Instant) -> Pushed {
        let event = event.into();
        if !self.active.load(Ordering::Acquire) {
            return Pushed::Diverted;
        }
        if event.is_stream() {
            if self.stream.try_push(event).is_err() {
                self.stream_lost.fetch_add(1, Ordering::Relaxed);
            }
            return Pushed::Diverted;
        }
        if event.is_jr() {
            return Pushed::Diverted;
        }
        if event.message_type() == SYSEX7
            && self.hear_sysex.load(Ordering::Relaxed)
            && self.stream.try_push(event).is_err()
        {
            self.stream_lost.fetch_add(1, Ordering::Relaxed);
        }
        if self.input.push(event, timestamp) {
            Pushed::Sent
        } else {
            Pushed::Full
        }
    }

    /// The identity of the port this handle feeds, and the slot it holds
    /// until it drops: the key to pass to
    /// [`HardwareMidiInputs`](super::HardwareMidiInputs)' `set_port_active` or
    /// `remove_input_port`.
    #[must_use]
    pub fn slot(&self) -> DeviceSlot {
        self.input.slot()
    }
}

/// What [`InputProducerHandle::push`] did with an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[must_use = "`Full` is a lost event: a caller counts or logs it"]
pub enum Pushed {
    /// The event went into the device's inlet, for the graph.
    Sent,
    /// The event was kept out of the graph on purpose (a UMP Stream message,
    /// a JR message, or any event of a paused port): nothing was lost.
    Diverted,
    /// The device's inlet had no room: the event is dropped, and counted as
    /// lost.
    Full,
}

/// The UMP Stream messages of one port, drained as they are read
/// (`HardwareMidiInputs::drain_stream`):
/// one ring lock per step, none held across the caller's work.
#[derive(Debug)]
pub struct StreamDrain {
    port: Option<Arc<HardwareMidiInput>>,
}

impl StreamDrain {
    pub(super) fn new(port: Option<Arc<HardwareMidiInput>>) -> Self {
        Self { port }
    }
}

impl Iterator for StreamDrain {
    type Item = Ump;

    fn next(&mut self) -> Option<Ump> {
        self.port.as_ref()?.pop_stream()
    }
}

/// A hardware **input** port in the list: its name, whether it delivers, and
/// the counts of its inlet. There is no output counterpart here: outbound MIDI
/// rides an event tap to a [`MidiOut`](tutti_midi_runtime::MidiOut) sink.
pub(crate) struct HardwareMidiInput {
    name: String,
    active: Arc<AtomicBool>,
    counts: InletCounts,
    /// The reading end of the stream ring; control side, one reader at a time.
    stream: Mutex<HeapCons<Ump>>,
    stream_lost: Arc<AtomicU64>,
    hear_sysex: Arc<AtomicBool>,
}

impl HardwareMidiInput {
    /// A port named `name` over `input`, and the producer handle for it.
    pub(crate) fn new(name: impl Into<String>, input: DeviceInput) -> (Self, InputProducerHandle) {
        let active = Arc::new(AtomicBool::new(true));
        let stream_lost = Arc::new(AtomicU64::new(0));
        let hear_sysex = Arc::new(AtomicBool::new(false));
        let (stream_in, stream_out) = HeapRb::<Ump>::new(STREAM_CAPACITY).split();
        (
            Self {
                name: name.into(),
                active: Arc::clone(&active),
                counts: input.counts(),
                stream: Mutex::new(stream_out),
                stream_lost: Arc::clone(&stream_lost),
                hear_sysex: Arc::clone(&hear_sysex),
            },
            InputProducerHandle {
                input,
                active,
                stream: stream_in,
                stream_lost,
                hear_sysex,
            },
        )
    }

    /// What the inlet has lost.
    pub(crate) fn losses(&self) -> Losses {
        self.counts.losses()
    }

    /// How many Stream messages the ring had no room for.
    pub(crate) fn stream_lost(&self) -> u64 {
        self.stream_lost.load(Ordering::Relaxed)
    }

    /// Whether the stream ring also takes a copy of the SysEx7 packets.
    pub(crate) fn set_hear_sysex(&self, hear: bool) {
        self.hear_sysex.store(hear, Ordering::Relaxed);
    }

    /// The oldest Stream message held, if any.
    pub(crate) fn pop_stream(&self) -> Option<Ump> {
        self.stream
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .try_pop()
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
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
