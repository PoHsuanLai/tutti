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

/// Push MIDI events at a single **terminal sink** — the write-side complement to
/// [`MidiIn`]. A sink is already addressed: it *is* the destination (a per-unit
/// mailbox, a hardware wire), so `queue` carries no id. Consuming units drain
/// the delivered events via [`MidiIn::poll_into`].
///
/// Implementations must be **lock-free** and **alloc-free** — `queue` runs on
/// the audio thread, once per routed event.
///
/// To deliver to *one of many* sinks selected by id (a fan-out bus), use
/// [`MidiRouter`] instead — the id belongs to the routing step, not the sink.
pub trait MidiOut: Send + Sync {
    /// Deliver `events`; return how many the sink **accepted**.
    ///
    /// `< events.len()` means the rest were dropped — a full ring, a device that
    /// refused the write. Dropping a note-off whose note-on landed is what
    /// produces a stuck note, so a caller with anywhere to report it must.
    /// A sink that cannot fail returns `events.len()`.
    ///
    /// The count is here rather than on the implementations alone because it
    /// used to be: `MidiSender::queue` computed one and the `()` trait impl threw
    /// it away, so the capable path and the obvious path differed with nothing to
    /// signal which was which — and `MidiSession::send` reported
    /// `events.len()` for a device that had refused every one.
    ///
    /// A partial accept is a **prefix**, not a subset: an implementation that
    /// hits a failure stops there rather than skipping and continuing, so the
    /// count always names an unbroken run and the stream stays in order.
    fn queue(&self, events: &[MidiEvent]) -> usize;
}

/// Route MIDI events to a registered sink selected by [`MidiUnitId`] — a fan-out
/// over many [`MidiOut`] sinks. The id names *which* sink.
///
/// Distinct from [`MidiOut`] (a single terminal sink, no id): a `MidiRouter`
/// owns the address→sink map and does the lookup. Implementations must be
/// **lock-free** and **alloc-free** — `queue` runs on the audio thread.
pub trait MidiRouter: Send + Sync {
    /// Deliver `events` to the sink registered under `unit_id`.
    ///
    /// Returns how many were accepted. `< events.len()` means the rest were
    /// **dropped**, for either of two reasons the count deliberately does not
    /// distinguish, because a caller acts on both the same way:
    ///
    /// - **unknown id** — nothing is registered under it (0 accepted). Routing
    ///   to an absent unit is legitimate, not an error.
    /// - **sink full** — the destination ring had no room. Dropping a note-off
    ///   whose note-on landed is what produces a stuck note.
    ///
    /// The return exists because this is the *ergonomic* path — the one most
    /// callers reach for — while [`MidiOut`] implementations one layer down
    /// already report their accepted count. A `()` here threw that away at
    /// exactly the boundary a consumer touches, leaving the capable path and the
    /// obvious path different with nothing to signal which was which.
    fn queue(&self, unit_id: MidiUnitId, events: &[MidiEvent]) -> usize;
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
    fn poll_into(&self, unit_id: MidiUnitId, block_size: usize, buffer: &mut [MidiEvent]) -> usize;
}

/// Drain everything that has arrived at one **pre-routing** input edge for this
/// block.
///
/// The events are not yet addressed: routing has not run, so nothing here knows
/// which unit any of them belongs to — deciding that is what the consumer does
/// with what it gets back. Exactly one owner drains this per block; a second
/// drainer takes events the first will never see.
///
/// This is the read-side complement to [`MidiOut`]: one undifferentiated stream,
/// no id. To read *one of many* streams selected by id, use [`MidiUnitSource`].
///
/// `block_size` is the frame count of the upcoming audio block. An edge that
/// converts arrival timestamps into offsets uses it, and every returned event's
/// `frame_offset` lies in `[0, block_size)`.
///
/// Implementations must be **lock-free** and **alloc-free** — this runs on the
/// audio thread, once per block.
pub trait MidiSource: Send + Sync {
    /// Write this block's pending events into `buffer`; return how many.
    ///
    /// **A full `buffer` drops the overflow.** An implementation draining a
    /// hardware ring has already consumed those events by the time it finds
    /// there is no room, and holding them back would need a stash outliving the
    /// call — so the contract is truncation, not deferral. Size `buffer` for the
    /// largest burst worth surviving.
    fn poll_block(&self, block_size: usize, buffer: &mut [MidiEvent]) -> usize;
}

/// Read the events addressed to one [`MidiUnitId`] out of a store that holds
/// **many** units' streams.
///
/// The id is a selector, not a filter: a store polled for unit A must leave unit
/// B's stream untouched, so one store feeds every unit reading from it. This is
/// the read-side twin of [`MidiRouter`] — same fan-out, opposite direction — and
/// the post-routing counterpart to [`MidiSource`], whose caller has no id to
/// give because routing has not run yet.
///
/// `block_size` is the frame count of the upcoming audio block; a beat-domain
/// store uses it to place each event's `frame_offset`, which must lie in
/// `[0, block_size)`.
///
/// Implementations must be **lock-free** and **alloc-free** — this runs on the
/// audio thread, once per block per unit.
pub trait MidiUnitSource: Send + Sync {
    /// Write `unit_id`'s events for this block into `buffer`; return how many.
    fn poll_unit(
        &self,
        unit_id: MidiUnitId,
        block_size: usize,
        buffer: &mut [MidiEvent],
    ) -> usize;
}
