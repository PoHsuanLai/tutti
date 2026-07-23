//! The audio-thread MIDI plumbing traits — two roles on the hot path:
//!
//! - [`MidiOut`] — the **push** side: routing code hands events to a routing
//!   address (a per-unit inbox, a fan-out bus, a plugin's out-port).
//! - [`MidiIn`] — the **pull** side: whoever needs events drains them during
//!   `process()`. Implemented by per-unit inboxes, clip players, export
//!   snapshots, and the hardware input edge alike — a hardware source is just a
//!   `MidiIn` that ignores the unit id and returns everything pending, its
//!   ring-buffer + timestamp→`frame_offset` machinery private to its impl.
//!
//! Both are `MidiEvent` + [`MidiUnitId`] plumbing; they live together because
//! they *are* the routing hot path. A unit's routing address (its
//! [`MidiUnitId`]) is exposed by each unit's own inherent `midi_unit_id()`.

use crate::ump::MidiEvent;
use crate::unit_id::MidiUnitId;

// -----------------------------------------------------------------------------
// Delivery: write side (out) and read side (in)
// -----------------------------------------------------------------------------

/// Deliver MIDI events to registered audio units — the write-side complement to
/// [`MidiIn`]. Routing code on the audio thread calls [`MidiOut::queue`]
/// to hand events to per-unit queues, which the consuming units drain via
/// [`MidiIn::poll_into`].
///
/// Implementations must be **lock-free** and **alloc-free** — `queue` is called
/// on the audio thread, once per routed event. Events for unknown unit ids
/// should be silently dropped.
pub trait MidiOut: Send + Sync {
    fn queue(&self, unit_id: MidiUnitId, events: &[MidiEvent]);
}

/// Pull MIDI events in the audio thread — the read-side complement to
/// [`MidiOut`].
///
/// Audio units store `Option<Box<dyn MidiIn>>` and call `poll_into()`
/// during `tick()`/`process()`. The concrete implementation determines whether
/// events come from a live registry (destructively draining SPSC channels), an
/// export snapshot, or a beat-scheduled clip player.
///
/// `block_size` describes the audio block currently being rendered. Schedulers
/// use it to convert beat-domain events into per-block `MidiEvent::frame_offset`
/// values that the consuming unit's `process()` loop can split on for
/// sample-accurate timing (the beat position comes from the source's own
/// `Timeline`, not from an absolute sample count). Live sources that stamp
/// `frame_offset` themselves (e.g. the lock-free MIDI registry) ignore it and
/// pass the queue contents through unchanged.
pub trait MidiIn: Send + Sync {
    /// Poll available MIDI events for the given unit into the buffer.
    ///
    /// Returns the number of events written to `buffer`. Each event's
    /// `frame_offset` field must lie in `[0, block_size)`. Zero allocations, no
    /// blocking — safe for the audio thread.
    fn poll_into(
        &self,
        unit_id: MidiUnitId,
        block_size: usize,
        buffer: &mut [MidiEvent],
    ) -> usize;
}
