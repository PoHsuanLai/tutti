//! Hot-path MIDI event delivery.
//!
//! The [`MidiQueue`] trait is the write-side complement to [`MidiSource`].
//! Routing code on the audio thread calls [`MidiQueue::queue`] to hand events
//! to per-unit queues, which the consuming audio units then drain via
//! `MidiSource::poll_into`.
//!
//! [`MidiSource`]: crate::source::MidiSource
//! [`MidiQueue::queue`]: MidiQueue::queue

use crate::ump::MidiEvent;
use crate::unit_id::MidiUnitId;

/// Deliver MIDI events to registered audio units.
///
/// Implementations must be **lock-free** and **alloc-free** — `queue` is
/// called on the audio thread, once per routed event. Events for unknown
/// unit ids should be silently dropped.
pub trait MidiQueue: Send + Sync {
    fn queue(&self, unit_id: MidiUnitId, events: &[MidiEvent]);
}
