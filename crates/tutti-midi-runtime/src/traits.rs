//! [`MidiOut`]: the terminal sink MIDI events are pushed at. The input
//! direction has no trait: a device's events reach the graph through an event
//! inlet ([`DeviceInlets`](crate::DeviceInlets)).
//!
//! The sink carries no address: an endpoint *is* its stream. Which node hears
//! what is the graph's wiring (a `MidiIngestNode` in `tutti-midi-runtime`
//! sends each channel out of its own event port).

use tutti_graph::UmpAt;

use crate::Outgoing;

// -----------------------------------------------------------------------------
// Delivery: write side (out) and read side (in)
// -----------------------------------------------------------------------------

/// A single **terminal sink** that MIDI events are pushed to.
///
/// A sink is already addressed: it *is* the destination (a
/// mailbox, a hardware wire), so `queue` carries no id. What was delivered
/// is read from the sink's own reader.
///
/// Implementations must be **lock-free** and **alloc-free** — `queue` runs on
/// the audio thread, once per event.
pub trait MidiOut: Send + Sync {
    /// Delivers `events` and returns how many the sink **accepted**.
    ///
    /// `< events.len()` means the rest were dropped — a full ring, a device that
    /// refused the write. Dropping a note-off whose note-on landed is what
    /// produces a stuck note, so a caller with anywhere to report it must.
    /// A sink that cannot fail returns `events.len()`.
    ///
    /// A partial accept is a **prefix**, not a subset: an implementation that
    /// hits a failure stops there rather than skipping and continuing, so the
    /// count always names an unbroken run and the stream stays in order.
    fn queue(&self, events: &[UmpAt]) -> usize;

    /// [`queue`](Self::queue) for events that say when to send
    /// ([`SendAt`](crate::SendAt)). A sink that schedules (a hardware output)
    /// overrides it and sends each at its time; the default hands the events
    /// to `queue` and ignores the time, which is every sink that has no wire
    /// clock (a mailbox, a test's). Same count and prefix semantics.
    fn queue_timed(&self, events: &[Outgoing]) -> usize {
        // Chunked on the stack: no allocation for a sink that may be on the
        // audio thread.
        const CHUNK: usize = 64;
        let mut accepted = 0;
        for chunk in events.chunks(CHUNK) {
            let mut buf = [(tutti_graph::Offset::ZERO, tutti_ump::Ump([0; 4])); CHUNK];
            for (slot, e) in buf.iter_mut().zip(chunk) {
                *slot = (e.at, e.ump);
            }
            let taken = self.queue(&buf[..chunk.len()]);
            accepted += taken;
            if taken < chunk.len() {
                break;
            }
        }
        accepted
    }
}
