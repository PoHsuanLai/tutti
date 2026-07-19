//! Unified MIDI event sourcing for audio units.
//!
//! The [`MidiSource`] trait provides a single abstraction for pulling MIDI
//! events in the audio thread. Two implementations are typical:
//!
//! - **Live**: a lock-free registry that destructively drains SPSC channels
//! - **Export / scheduled**: a cursor-based reader that emits beat-scheduled
//!   events with sample-accurate `frame_offset` set inside the requested block

use crate::ump::MidiEvent;
use crate::unit_id::MidiUnitId;

/// Trait for pulling MIDI events in the audio thread.
///
/// Audio units store `Option<Box<dyn MidiSource>>` and call `poll_into()`
/// during `tick()`/`process()`. The concrete implementation determines
/// whether events come from a live registry, an export snapshot, or a
/// beat-scheduled clip player.
///
/// `block_start_sample` and `block_size` describe the audio block currently
/// being rendered. Schedulers use them to convert beat-domain events into
/// per-block `MidiEvent::frame_offset` values that the consuming unit's
/// `process()` loop can split on for sample-accurate timing.
///
/// Live sources that don't track absolute sample positions (e.g. the lock-
/// free MIDI registry, where producers stamp `frame_offset` themselves)
/// can ignore both arguments and pass the queue contents through unchanged.
pub trait MidiSource: Send + Sync {
    /// Poll available MIDI events for the given unit into the buffer.
    ///
    /// Returns the number of events written to `buffer`. Each event's
    /// `frame_offset` field must lie in `[0, block_size)`. Zero
    /// allocations, no blocking — safe for the audio thread.
    fn poll_into(
        &self,
        unit_id: MidiUnitId,
        block_start_sample: u64,
        block_size: usize,
        buffer: &mut [MidiEvent],
    ) -> usize;
}
