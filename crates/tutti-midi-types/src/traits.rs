//! The audio-thread MIDI plumbing traits, one per direction: [`MidiOut`]
//! pushes events at a sink, [`MidiIn`] drains what arrived at an input edge.
//! Both are lock-free and alloc-free, called once per block or per event.
//!
//! Neither carries an address: an endpoint *is* its stream. Which node hears
//! what is the graph's wiring (a `MidiInputNode` in `tutti-midi-runtime`
//! sends each channel out of its own event port).

use crate::ump::MidiEvent;

// -----------------------------------------------------------------------------
// Delivery: write side (out) and read side (in)
// -----------------------------------------------------------------------------

/// A single **terminal sink** that MIDI events are pushed to.
///
/// The write-side complement to [`MidiIn`]. A sink is already addressed: it *is* the destination (a
/// mailbox, a hardware wire), so `queue` carries no id. What was delivered
/// is drained with [`MidiIn::poll_block`] or the sink's own reader.
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
    fn queue(&self, events: &[MidiEvent]) -> usize;
}

/// One input edge's per-node read state, which the audio thread polls once per block.
///
/// The events are not yet addressed: nothing here knows
/// which unit any of them belongs to — deciding that is what the consumer does
/// with what it gets back. Exactly one owner drains an edge per block; a second
/// drainer takes events the first will never see.
///
/// This is the read-side complement to [`MidiOut`]: one undifferentiated stream,
/// no id.
///
/// **The node owns it** (`Box<dyn MidiIn>`), and polls it through `&mut self`:
/// whatever state a poll keeps between blocks (a drain's scratch, its read of
/// a published port set) belongs to the one node that polls, built with it
/// on the control thread. A shared edge (a hardware port manager) hands out
/// one such poller per node rather than being polled through a shared
/// reference.
///
/// `block_size` is the frame count of the upcoming audio block. An edge that
/// converts arrival timestamps into offsets uses it, and every returned event's
/// `frame_offset` lies in `[0, block_size)`.
///
/// Implementations must be **lock-free** and **alloc-free** — this runs on the
/// audio thread, once per block.
pub trait MidiIn: Send {
    /// Writes this block's pending events into `buffer` and returns how many.
    ///
    /// **A full `buffer` drops the overflow.** An implementation draining a
    /// hardware ring has already consumed those events by the time it finds
    /// there is no room, and holding them back would need a stash outliving the
    /// call — so the contract is truncation, not deferral. Size `buffer` for the
    /// largest burst worth surviving.
    fn poll_block(&mut self, block_size: usize, buffer: &mut [MidiEvent]) -> usize;
}
