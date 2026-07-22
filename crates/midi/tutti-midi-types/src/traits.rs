//! The audio-thread MIDI plumbing traits: how events enter, get delivered to,
//! and are pulled by audio units.
//!
//! Small traits that work together on the hot path:
//! - [`MidiInputSource`] — produces raw `(port, event)` pairs from hardware /
//!   virtual / Web-MIDI sources, frame offsets already computed.
//! - [`MidiQueue`] — the write side: routing code hands events to per-unit queues.
//! - [`MidiSource`] — the read side: a unit drains its queue into a buffer during
//!   `process()`.
//!
//! All are `MidiEvent` + [`MidiUnitId`] plumbing; they live together because
//! they *are* the routing hot path. A unit's routing address (its
//! [`MidiUnitId`]) is exposed by each unit's own inherent `midi_unit_id()`.

use crate::ump::MidiEvent;
use crate::unit_id::MidiUnitId;

// -----------------------------------------------------------------------------
// Input edge
// -----------------------------------------------------------------------------

/// RT-safe MIDI input source that can be polled from the audio callback.
///
/// Implementations must be lock-free and safe to call from the audio thread.
/// `cycle_read` is called once per audio buffer to collect all pending MIDI
/// events. This abstraction lets the audio callback read from various sources
/// (hardware ports, virtual ports, WASM Web MIDI, …) without depending on
/// specific implementations or platform timestamps — implementations convert
/// platform-specific timestamps (e.g. `Instant`, `performance.now()`) into
/// `frame_offset` internally.
pub trait MidiInputSource: Send + Sync {
    /// Returns `(port_index, event)` tuples with `event.frame_offset` already set.
    /// Valid until the next call.
    fn cycle_read(&self, nframes: usize) -> &[(usize, MidiEvent)];
}

/// No-op source for when MIDI hardware is not connected.
#[derive(Debug, Default)]
pub struct NoMidiInput;

impl MidiInputSource for NoMidiInput {
    fn cycle_read(&self, _nframes: usize) -> &[(usize, MidiEvent)] {
        &[]
    }
}

// -----------------------------------------------------------------------------
// Delivery: write side (queue) and read side (source)
// -----------------------------------------------------------------------------

/// Deliver MIDI events to registered audio units — the write-side complement to
/// [`MidiSource`]. Routing code on the audio thread calls [`MidiQueue::queue`]
/// to hand events to per-unit queues, which the consuming units drain via
/// [`MidiSource::poll_into`].
///
/// Implementations must be **lock-free** and **alloc-free** — `queue` is called
/// on the audio thread, once per routed event. Events for unknown unit ids
/// should be silently dropped.
pub trait MidiQueue: Send + Sync {
    fn queue(&self, unit_id: MidiUnitId, events: &[MidiEvent]);
}

/// Pull MIDI events in the audio thread — the read-side complement to
/// [`MidiQueue`].
///
/// Audio units store `Option<Box<dyn MidiSource>>` and call `poll_into()`
/// during `tick()`/`process()`. The concrete implementation determines whether
/// events come from a live registry (destructively draining SPSC channels), an
/// export snapshot, or a beat-scheduled clip player.
///
/// `block_start_sample` and `block_size` describe the audio block currently
/// being rendered. Schedulers use them to convert beat-domain events into
/// per-block `MidiEvent::frame_offset` values that the consuming unit's
/// `process()` loop can split on for sample-accurate timing. Live sources that
/// don't track absolute sample positions (e.g. the lock-free MIDI registry,
/// where producers stamp `frame_offset` themselves) can ignore both arguments
/// and pass the queue contents through unchanged.
pub trait MidiSource: Send + Sync {
    /// Poll available MIDI events for the given unit into the buffer.
    ///
    /// Returns the number of events written to `buffer`. Each event's
    /// `frame_offset` field must lie in `[0, block_size)`. Zero allocations, no
    /// blocking — safe for the audio thread.
    fn poll_into(
        &self,
        unit_id: MidiUnitId,
        block_start_sample: u64,
        block_size: usize,
        buffer: &mut [MidiEvent],
    ) -> usize;
}

