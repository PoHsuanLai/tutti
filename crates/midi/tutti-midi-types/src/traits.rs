//! The audio-thread MIDI plumbing traits — four, in two pairs, one pair per
//! direction. Each pair splits on **arity**: does this touch one stream, or one
//! of many selected by a [`MidiUnitId`]?
//!
//! |            | one stream  | one of many, by id |
//! |------------|-------------|--------------------|
//! | **push**   | [`MidiOut`] | [`MidiRouter`]     |
//! | **pull**   | [`MidiIn`] | [`MidiUnitIn`] |
//!
//! The id is what separates the columns, and it is not decoration: a
//! single-stream endpoint *is* its address, so passing one would be meaningless,
//! while a fan-out cannot answer without one.
//!
//! Do not collapse the read side back into one trait carrying an optional
//! `unit_id`. A single-stream implementor has nothing to compare the id against,
//! so it either re-checks an id it already owns or ignores it — and the hardware
//! edge then has to be polled with a `MidiUnitId::new(0)` sentinel, which is a
//! *real* id: install a per-unit source at that seam and the whole hardware
//! stream silently goes to it. Splitting on arity drops the parameter where it is
//! noise and keeps it where it selects.
//!
//! All four live together because they *are* the routing hot path: lock-free,
//! alloc-free, called once per block. A unit's routing address is exposed by its
//! own inherent `midi_unit_id()`.

use crate::ump::MidiEvent;
use crate::unit_id::MidiUnitId;

use std::sync::Arc;

use tutti_types::SampleRate;

// -----------------------------------------------------------------------------
// Delivery: write side (out) and read side (in)
// -----------------------------------------------------------------------------

/// Push MIDI events at a single **terminal sink** — the write-side complement to
/// [`MidiIn`]. A sink is already addressed: it *is* the destination (a per-unit
/// mailbox, a hardware wire), so `queue` carries no id. Consuming units drain
/// the delivered events via [`MidiIn::poll_block`] or
/// [`MidiUnitIn::poll_unit`].
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
    /// The count belongs on the trait, not on individual implementations: leave
    /// it off and a capable sink computes a real figure while a trivial one
    /// discards it, so the two paths disagree with nothing to signal which is
    /// which — and a session layer ends up reporting `events.len()` for a device
    /// that refused every event.
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

/// Drain everything that has arrived at one **pre-routing** input edge for this
/// block.
///
/// The events are not yet addressed: routing has not run, so nothing here knows
/// which unit any of them belongs to — deciding that is what the consumer does
/// with what it gets back. Exactly one owner drains this per block; a second
/// drainer takes events the first will never see.
///
/// This is the read-side complement to [`MidiOut`]: one undifferentiated stream,
/// no id. To read *one of many* streams selected by id, use [`MidiUnitIn`].
///
/// `block_size` is the frame count of the upcoming audio block. An edge that
/// converts arrival timestamps into offsets uses it, and every returned event's
/// `frame_offset` lies in `[0, block_size)`.
///
/// Implementations must be **lock-free** and **alloc-free** — this runs on the
/// audio thread, once per block.
pub trait MidiIn: Send + Sync {
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
/// the post-routing counterpart to [`MidiIn`], whose caller has no id to
/// give because routing has not run yet.
///
/// `block_size` is the frame count of the upcoming audio block; a beat-domain
/// store uses it to place each event's `frame_offset`, which must lie in
/// `[0, block_size)`.
///
/// Implementations must be **lock-free** and **alloc-free** — this runs on the
/// audio thread, once per block per unit.
pub trait MidiUnitIn: Send + Sync {
    /// Write `unit_id`'s events for this block into `buffer`; return how many.
    fn poll_unit(&self, unit_id: MidiUnitId, block_size: usize, buffer: &mut [MidiEvent]) -> usize;

    /// A copy of this source for an **offline render**: addressed to `unit`
    /// (the forked unit's port, which is not the live one's), reading the
    /// render's timeline out of `ctx` instead of the live transport, and
    /// sharing no cursor with this one. Control thread, not the audio path.
    ///
    /// `ctx` is what `AudioUnit::rebind_offline` is handed — today a
    /// `&OfflineTransport` (tutti-core); a source downcasts it, and a
    /// context of any other type rebinds nothing (`None`).
    ///
    /// `None` (the default) for a source that is not a function of a
    /// timeline — a live inbox, an already-offline snapshot — so a fork of
    /// the unit it feeds carries no source rather than one reading the live
    /// playhead. Why this exists: a graph fork (design doc 013, `Editor::fork`)
    /// gives a MIDI-driven unit a fresh port, and an exported instrument
    /// whose clip did not come with it renders silence.
    fn rebind_offline(
        &self,
        unit: MidiUnitId,
        ctx: &dyn core::any::Any,
    ) -> Option<Arc<dyn MidiUnitIn>> {
        let _ = (unit, ctx);
        None
    }

    /// The sample rate the source places events at changed — the unit it
    /// feeds was re-prepared (a device change, or a fork prepared at an
    /// export's rate). A beat-domain source converts beats to frame offsets
    /// at this rate. Lock-free; the default ignores it, for a source with no
    /// rate of its own.
    fn set_sample_rate(&self, sample_rate: SampleRate) {
        let _ = sample_rate;
    }
}
