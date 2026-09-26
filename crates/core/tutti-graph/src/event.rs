//! Events: what flows on an event port.
//!
//! Doc 013 §1 ("Port kinds") and owner decision 4: events are **graph ports**,
//! not a side channel, so the one compiler pass that aligns audio (PDC) aligns
//! notes and automation too. Decision 6: an event input port may have several
//! sources, merged deterministically by `(offset, source order)`, where source
//! order is the source port's `(NodeKey, port)` — layering a keyboard and a
//! clip is the normal case, not an edge case. Decision 7:
//! automation is carried as **linear ramp** events first.
//!
//! # Why a raw UMP payload and not `tutti-midi-types`
//!
//! `tutti_midi_types::MidiEvent` is the right *shape* (a frame offset plus four
//! UMP words), but the crate links `midi2`, `midly`, `bitflags` and `thiserror`,
//! and this crate sits below everything that would ever build a node. [`Ump`]
//! carries the same four words, so converting at a node's edge is a copy of 16
//! bytes, and the graph does not link a MIDI codec to move them.

use std::cell::Cell;

#[cfg(doc)]
use tutti_types::UnitParam;
use tutti_types::{ParamAddr, ParamKey, Samples, Unit};

use crate::time::Offset;

/// Four raw Universal MIDI Packet words.
///
/// The meaningful prefix is determined by the message type in the first word's
/// top nibble, exactly as `tutti_midi_types::MidiEvent::data_words` reads it;
/// unused words are zero. The graph never inspects them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Ump(pub [u32; 4]);

/// A linear ramp of one parameter to a target, starting at the event's offset.
///
/// Addressed by [`ParamAddr`], so it reaches both tutti's own parameters and a
/// foreign unit's (a hosted plugin's numeric id) without a later breaking
/// change:
///
/// - **Built-in params** are built from a typed [`ParamKey<U>`] and a `U`,
///   and read back the same way, so the unit is checked at both ends. In
///   between the value travels type-erased (the stable [`UnitParam`] id and
///   the raw `f32`), because one event port carries ramps for parameters of
///   different units and an `Event` has to be one `Copy` type. The raw value
///   is private: nothing reads a built-in ramp without naming its key.
/// - **Foreign params** ([`ParamRamp::foreign`]) carry an opaque `u32` id and
///   a raw value in the foreign unit's own normalisation. That is the C ABI
///   boundary the units rule stops at: tutti does not know the unit.
///
/// ```
/// use tutti_graph::ParamRamp;
/// use tutti_types::{Db, Hz, ParamKey, Samples};
/// let r = ParamRamp::new(ParamKey::<Hz>::CUTOFF, Hz(800.0), Samples(64));
/// assert_eq!(r.target(ParamKey::<Hz>::CUTOFF), Some(Hz(800.0)));
/// assert_eq!(r.target(ParamKey::<Db>::THRESHOLD), None); // another param
/// assert_eq!(r.foreign_target(7), None); // not a foreign param at all
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParamRamp {
    addr: ParamAddr,
    target: f32,
    duration: Samples,
}

impl ParamRamp {
    /// Ramp built-in parameter `key` to `target` over `duration` (zero is a
    /// step).
    pub fn new<U: Unit<Raw = f32>>(key: ParamKey<U>, target: U, duration: Samples) -> Self {
        Self {
            addr: ParamAddr::Unit(key.id()),
            target: target.to_raw(),
            duration,
        }
    }

    /// Ramp a foreign unit's parameter `id` to the raw `target` over
    /// `duration`. The value is in the foreign unit's own terms (see the type
    /// docs).
    pub fn foreign(id: u32, target: f32, duration: Samples) -> Self {
        Self {
            addr: ParamAddr::Id(id),
            target,
            duration,
        }
    }

    /// Which parameter this ramps — for routing on the address.
    pub fn addr(&self) -> ParamAddr {
        self.addr
    }

    /// The target, in `key`'s unit, if this ramp is for `key`.
    pub fn target<U: Unit<Raw = f32>>(&self, key: ParamKey<U>) -> Option<U> {
        (self.addr == ParamAddr::Unit(key.id())).then(|| U::from_raw(self.target))
    }

    /// The raw target, if this ramp is for foreign parameter `id`.
    pub fn foreign_target(&self, id: u32) -> Option<f32> {
        (self.addr == ParamAddr::Id(id)).then_some(self.target)
    }

    /// How long the ramp takes.
    pub fn duration(&self) -> Samples {
        self.duration
    }
}

/// What an [`Event`] says.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EventKind {
    /// A MIDI message, as UMP words.
    Midi(Ump),
    /// A parameter automation ramp.
    Ramp(ParamRamp),
}

/// One event on an event port: an offset into the current block, and what
/// happens there.
///
/// `offset` is an [`Offset`] — a position inside the block the event is
/// delivered in, never an absolute [`Frame`](tutti_types::Frame) (see the
/// `time` module docs, `src/time.rs`). Slices of events handed to a node are
/// sorted by it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Event {
    /// Where in the block.
    pub offset: Offset,
    /// The payload.
    pub kind: EventKind,
}

// An event is what every event buffer, merge and delay FIFO holds by value,
// preallocated on the control side and copied on the audio thread: it must
// stay `Copy` and small, whatever payload is added (a curve segment, say).
// Four UMP words cover every MIDI 2.0 message, 128-bit ones included, so a
// MIDI payload never needs to grow it. Asserted here rather than discovered
// as a slower merge.
const _: () = {
    const fn copy<T: Copy>() {}
    copy::<Event>();
    assert!(std::mem::size_of::<Event>() <= 32);
};

impl Event {
    /// A MIDI event at `offset`.
    pub const fn midi(offset: Offset, words: [u32; 4]) -> Self {
        Self {
            offset,
            kind: EventKind::Midi(Ump(words)),
        }
    }

    /// Whether this is a MIDI note-off: a MIDI 1.0 channel-voice note-off, a
    /// note-on at velocity 0 (the MIDI 1.0 spelling of the same thing), or a
    /// MIDI 2.0 channel-voice note-off. The one event the graph refuses to
    /// lose — see the delay FIFO's overflow policy.
    pub fn is_note_off(&self) -> bool {
        let EventKind::Midi(Ump(w)) = self.kind else {
            return false;
        };
        let mt = w[0] >> 28;
        let status = (w[0] >> 20) & 0xf;
        match mt {
            0x2 => status == 0x8 || (status == 0x9 && w[0] & 0x7f == 0),
            0x4 => status == 0x8,
            _ => false,
        }
    }

    /// A parameter ramp starting at `offset`.
    pub const fn ramp(offset: Offset, ramp: ParamRamp) -> Self {
        Self {
            offset,
            kind: EventKind::Ramp(ramp),
        }
    }
}

/// Why a slice is not a valid [`SortedEvents`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventOrderError {
    /// The event at this index has a smaller offset than the one before it.
    Unsorted {
        /// Index into the slice.
        at: usize,
    },
    /// The event at this index is at or past the end of the block.
    OutOfBlock {
        /// Index into the slice.
        at: usize,
    },
}

/// A block's events for one port: **sorted by offset, every offset inside the
/// block** — by construction.
///
/// The only ways to get one are [`new`](Self::new), which checks,
/// [`sort`](Self::sort), which sorts (stably, so equal offsets keep their
/// order; control side — the sort may allocate), and the executor, whose event
/// slots are sorted and in-block by the rules of [`EventWriter`] and the
/// merge. A node reading a `SortedEvents` never re-checks either property.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SortedEvents<'a> {
    events: &'a [Event],
}

impl<'a> SortedEvents<'a> {
    /// No events.
    pub const EMPTY: SortedEvents<'static> = SortedEvents { events: &[] };

    /// `events`, if they are sorted by offset and every offset is below
    /// `frames`.
    pub fn new(events: &'a [Event], frames: usize) -> Result<Self, EventOrderError> {
        for (i, e) in events.iter().enumerate() {
            if e.offset.index() >= frames {
                return Err(EventOrderError::OutOfBlock { at: i });
            }
            if i > 0 && events[i - 1].offset > e.offset {
                return Err(EventOrderError::Unsorted { at: i });
            }
        }
        Ok(Self { events })
    }

    /// Sort `events` in place by offset (stably), then view them. Fails only
    /// when an offset is outside the block. Control side: may allocate.
    pub fn sort(events: &'a mut [Event], frames: usize) -> Result<Self, EventOrderError> {
        if let Some(at) = events.iter().position(|e| e.offset.index() >= frames) {
            return Err(EventOrderError::OutOfBlock { at });
        }
        events.sort_by_key(|e| e.offset);
        Ok(Self { events })
    }

    /// Wrap a slice the executor's own rules keep sorted and in-block.
    /// Checked in debug builds.
    pub(crate) fn trusted(events: &'a [Event], frames: usize) -> Self {
        debug_assert_eq!(Self::new(events, frames).map(|_| ()), Ok(()));
        Self { events }
    }

    /// The events.
    pub fn as_slice(&self) -> &'a [Event] {
        self.events
    }

    /// Number of events.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// The events, in order.
    pub fn iter(&self) -> std::slice::Iter<'a, Event> {
        self.events.iter()
    }
}

/// A block split at its event offsets: the iterator
/// [`Io::sub_blocks`](crate::Io::sub_blocks) returns.
///
/// Yields `(range, events)` pairs that tile the block in order: `range` is a
/// run of frames (slice indices into the block's buffers), and `events` are
/// exactly the events at `range.start` — so a node that applies `events`,
/// then renders `range`, applies every event on its own frame. A block with
/// no events is one chunk; several events on one offset arrive together, in
/// their delivered order; an event on the last frame gets a one-frame chunk.
///
/// Zero allocation: it walks the event slice it was built from.
#[derive(Clone, Debug)]
pub struct SubBlocks<'a> {
    events: &'a [Event],
    pos: usize,
    frames: usize,
}

impl<'a> SortedEvents<'a> {
    /// Split a block of `frames` at these events' offsets. `frames` is the
    /// length these events were checked against.
    pub(crate) fn sub_blocks(self, frames: usize) -> SubBlocks<'a> {
        SubBlocks {
            events: self.events,
            pos: 0,
            frames,
        }
    }
}

impl<'a> Iterator for SubBlocks<'a> {
    type Item = (std::ops::Range<usize>, SortedEvents<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.frames {
            return None;
        }
        // Every event before `pos` was yielded with an earlier chunk, so the
        // ones at `pos` are the prefix; `<=` rather than `==` only so a
        // malformed slice could never stall the walk.
        let here = self
            .events
            .iter()
            .take_while(|e| e.offset.index() <= self.pos)
            .count();
        let (now, rest) = self.events.split_at(here);
        let end = rest
            .first()
            .map_or(self.frames, |e| e.offset.index())
            .min(self.frames);
        let range = self.pos..end;
        self.events = rest;
        self.pos = end;
        Some((range, SortedEvents { events: now }))
    }
}

impl std::iter::FusedIterator for SubBlocks<'_> {}

impl<'a> IntoIterator for SortedEvents<'a> {
    type Item = &'a Event;
    type IntoIter = std::slice::Iter<'a, Event>;
    fn into_iter(self) -> Self::IntoIter {
        self.events.iter()
    }
}

impl std::ops::Deref for SortedEvents<'_> {
    type Target = [Event];
    fn deref(&self) -> &[Event] {
        self.events
    }
}

/// Why [`EventWriter::push`] refused an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventRejected {
    /// The port's preallocated buffer is full. The event is dropped; the
    /// executor counts it (see `Executor::dropped_events`).
    Full,
    /// `offset` is at or past the end of the block.
    OutOfBlock,
    /// `offset` is before the previous event's. A port's events are delivered
    /// sorted, and sorting on the audio thread would mean either an allocating
    /// stable sort or an unstable one that reorders equal offsets — so the
    /// writer requires order instead of restoring it.
    OutOfOrder,
}

/// The preallocated sink a node writes one event output port through.
///
/// Never allocates: it refuses past its capacity rather than growing, and the
/// capacity was reserved on the control thread.
pub struct EventWriter<'a> {
    // `None` only for a detached writer, which the executor uses to fill the
    // unused tail of its fixed-size port table and never hands to a node.
    buf: Option<&'a mut Vec<Event>>,
    cap: usize,
    frames: u32,
    // A shared counter rather than a field: the executor reads it after the
    // node returns, while the writer itself is still borrowed by the `Io` the
    // node consumed.
    dropped: Option<&'a Cell<u32>>,
}

impl<'a> EventWriter<'a> {
    /// A writer over `buf`, which the caller has cleared. At most `cap` events
    /// are accepted, and only at offsets `< frames`.
    pub(crate) fn new(
        buf: &'a mut Vec<Event>,
        cap: usize,
        frames: u32,
        dropped: &'a Cell<u32>,
    ) -> Self {
        debug_assert!(buf.is_empty());
        Self {
            buf: Some(buf),
            cap,
            frames,
            dropped: Some(dropped),
        }
    }

    /// A writer that accepts nothing: a placeholder for an unused port slot.
    pub(crate) const fn detached() -> Self {
        Self {
            buf: None,
            cap: 0,
            frames: 0,
            dropped: None,
        }
    }

    fn reject(&self, why: EventRejected) -> Result<(), EventRejected> {
        if let Some(d) = self.dropped {
            d.set(d.get() + 1);
        }
        Err(why)
    }

    /// Append `event`. Offsets must be non-decreasing and inside the block.
    pub fn push(&mut self, event: Event) -> Result<(), EventRejected> {
        if event.offset.get() >= self.frames {
            return self.reject(EventRejected::OutOfBlock);
        }
        if self.len() >= self.cap {
            return self.reject(EventRejected::Full);
        }
        let Some(buf) = self.buf.as_deref_mut() else {
            return self.reject(EventRejected::Full);
        };
        if buf.last().is_some_and(|e| e.offset > event.offset) {
            return self.reject(EventRejected::OutOfOrder);
        }
        buf.push(event);
        Ok(())
    }

    /// How many events this port has accepted so far this block.
    pub fn len(&self) -> usize {
        self.buf.as_ref().map_or(0, |b| b.len())
    }

    /// Whether nothing has been written yet this block.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Merge `sources` (each sorted by offset) into `out`, ordered by
/// `(offset, source index)`. At most `cap` events land; the rest are counted.
///
/// A k-way merge rather than concatenate-and-sort: the stable sort that would
/// give the same order allocates, and the unstable one does not give it.
pub(crate) fn merge_into(sources: &[&[Event]], out: &mut Vec<Event>, cap: usize) -> u32 {
    let mut heads = [0usize; crate::MAX_PORTS];
    let heads = &mut heads[..sources.len()];
    let mut dropped = 0u32;
    loop {
        let mut best: Option<(Offset, usize)> = None;
        for (i, src) in sources.iter().enumerate() {
            if let Some(e) = src.get(heads[i]) {
                // Strict `<`: on a tie the lower source index keeps it.
                if best.is_none_or(|(o, _)| e.offset < o) {
                    best = Some((e.offset, i));
                }
            }
        }
        let Some((_, i)) = best else { break };
        let e = sources[i][heads[i]];
        heads[i] += 1;
        if out.len() < cap {
            out.push(e);
        } else {
            dropped += 1;
        }
    }
    dropped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(offset: u32, tag: u32) -> Event {
        Event::midi(Offset::raw(offset), [tag, 0, 0, 0])
    }

    /// Ties go to the earlier source; otherwise offset order.
    ///
    /// Mutation: `<` → `<=` in `merge_into` → the tie at offset 3 flips to
    /// source 1 first → fails.
    #[test]
    fn merge_orders_by_offset_then_source() {
        let a = [ev(0, 1), ev(3, 2), ev(9, 3)];
        let b = [ev(3, 10), ev(4, 11)];
        let mut out = Vec::with_capacity(8);
        let dropped = merge_into(&[&a, &b], &mut out, 8);
        assert_eq!(dropped, 0);
        let tags: Vec<u32> = out
            .iter()
            .map(|e| match e.kind {
                EventKind::Midi(Ump(w)) => w[0],
                EventKind::Ramp(_) => unreachable!(),
            })
            .collect();
        assert_eq!(tags, vec![1, 2, 10, 11, 3]);
    }

    /// The merge refuses past capacity and says how many it refused.
    ///
    /// Mutation: drop the `out.len() < cap` check → `out` grows to 5 → fails.
    #[test]
    fn merge_counts_what_did_not_fit() {
        let a = [ev(0, 1), ev(1, 2), ev(2, 3)];
        let b = [ev(0, 4), ev(5, 5)];
        let mut out = Vec::with_capacity(8);
        assert_eq!(merge_into(&[&a, &b], &mut out, 3), 2);
        assert_eq!(out.len(), 3);
    }

    /// `SortedEvents::new` refuses what is unsorted or out of the block;
    /// `sort` repairs order (stably) and still refuses out-of-block.
    ///
    /// Mutation: drop the `Unsorted` check in `new` → the unsorted slice is
    /// accepted → fails. Replace `sort_by_key` with `sort_unstable_by_key` →
    /// usually still passes here, which is why the tie case below checks the
    /// payload order explicitly on a slice long enough for the unstable sort
    /// to reorder.
    #[test]
    fn sorted_events_are_sorted_and_in_block() {
        let unsorted = [ev(3, 0), ev(1, 1)];
        assert_eq!(
            SortedEvents::new(&unsorted, 8),
            Err(EventOrderError::Unsorted { at: 1 })
        );
        let late = [ev(1, 0), ev(8, 1)];
        assert_eq!(
            SortedEvents::new(&late, 8),
            Err(EventOrderError::OutOfBlock { at: 1 })
        );
        assert!(SortedEvents::new(&[ev(0, 0), ev(0, 1), ev(7, 2)], 8).is_ok());

        let mut v: Vec<Event> = (0..64).map(|i| ev(i % 3, i)).collect();
        let sorted = SortedEvents::sort(&mut v, 8).expect("in block");
        let tags: Vec<u32> = sorted
            .iter()
            .map(|e| match e.kind {
                EventKind::Midi(Ump(w)) => w[0],
                EventKind::Ramp(_) => unreachable!(),
            })
            .collect();
        let mut want: Vec<u32> = (0..64).collect();
        want.sort_by_key(|i| i % 3);
        assert_eq!(tags, want, "stable: equal offsets keep their order");
        let mut late = vec![ev(9, 0)];
        assert!(SortedEvents::sort(&mut late, 8).is_err());
    }

    /// The writer enforces the three invariants a consumer relies on.
    ///
    /// Mutation: delete the `OutOfOrder` branch → the second push succeeds →
    /// fails.
    #[test]
    fn writer_rejects_out_of_order_out_of_block_and_overflow() {
        let mut buf = Vec::with_capacity(4);
        let dropped = Cell::new(0);
        let mut w = EventWriter::new(&mut buf, 2, 16, &dropped);
        assert_eq!(w.push(ev(5, 0)), Ok(()));
        assert_eq!(w.push(ev(4, 0)), Err(EventRejected::OutOfOrder));
        assert_eq!(w.push(ev(16, 0)), Err(EventRejected::OutOfBlock));
        assert_eq!(w.push(ev(5, 0)), Ok(()));
        assert_eq!(w.push(ev(6, 0)), Err(EventRejected::Full));
        assert_eq!(w.len(), 2);
        assert_eq!(dropped.get(), 3);
    }

    /// The splitter under test, written the obvious way: every distinct
    /// offset (and 0) starts a chunk, each chunk runs to the next start, and
    /// its events are those whose offset equals its start.
    fn hand_split(events: &[Event], frames: usize) -> Vec<(std::ops::Range<usize>, Vec<Event>)> {
        let mut starts: Vec<usize> = std::iter::once(0)
            .chain(events.iter().map(|e| e.offset.index()))
            .collect();
        starts.sort_unstable();
        starts.dedup();
        let ends = starts
            .iter()
            .skip(1)
            .copied()
            .chain(std::iter::once(frames));
        starts
            .iter()
            .zip(ends)
            .map(|(&a, b)| {
                let here = events
                    .iter()
                    .filter(|e| e.offset.index() == a)
                    .copied()
                    .collect();
                (a..b, here)
            })
            .collect()
    }

    fn split(events: &[Event], frames: usize) -> Vec<(std::ops::Range<usize>, Vec<Event>)> {
        SortedEvents::new(events, frames)
            .expect("sorted and in the block")
            .sub_blocks(frames)
            .map(|(r, e)| (r, e.to_vec()))
            .collect()
    }

    /// The edges `sub_blocks` has to get right, spelled out: an event at
    /// offset 0 (no empty leading chunk), several on one offset (one chunk,
    /// all of them, in order), one on the last frame (a one-frame chunk), and
    /// no events at all (the whole block).
    ///
    /// Mutation: in `SubBlocks::next`, split off only the *first* event at
    /// `pos` (`split_at(here.min(1))`) → the second event at offset 5 comes
    /// out in a zero-length chunk of its own → fails.
    #[test]
    fn sub_blocks_split_at_every_offset() {
        let evs = [ev(0, 1), ev(5, 2), ev(5, 3), ev(7, 4)];
        let got = split(&evs, 8);
        assert_eq!(
            got,
            vec![
                (0..5, vec![ev(0, 1)]),
                (5..7, vec![ev(5, 2), ev(5, 3)]),
                (7..8, vec![ev(7, 4)]),
            ]
        );
        assert_eq!(split(&[], 8), vec![(0..8, vec![])]);
        assert_eq!(
            split(&[ev(3, 9)], 8),
            vec![(0..3, vec![]), (3..8, vec![ev(3, 9)])]
        );
        assert_eq!(split(&evs, 8), hand_split(&evs, 8));
    }

    proptest::proptest! {
        /// Against the hand-rolled splitter, on random sorted offsets — with
        /// offset 0, the last frame and repeats all drawn often — and random
        /// block lengths including 1.
        ///
        /// Mutation: yield all but the last of several events that share an
        /// offset → the count and the comparison both fail.
        #[test]
        fn sub_blocks_match_a_hand_rolled_splitter(
            frames in 1usize..40,
            raw in proptest::collection::vec(0u32..6, 0..24),
        ) {
            // Bias toward the edges: 0 → offset 0, 5 → the last frame,
            // anything else → a frame in between (repeats are frequent).
            let last = frames as u32 - 1;
            let mut offsets: Vec<u32> = raw
                .iter()
                .enumerate()
                .map(|(i, &r)| match r {
                    0 => 0,
                    5 => last,
                    _ => (r * 7 + i as u32) % frames as u32,
                })
                .collect();
            offsets.sort_unstable();
            let evs: Vec<Event> = offsets
                .iter()
                .enumerate()
                .map(|(i, &o)| ev(o, i as u32))
                .collect();
            let got = split(&evs, frames);
            proptest::prop_assert_eq!(&got, &hand_split(&evs, frames));
            // The chunks tile the block and every event comes out once.
            let covered: usize = got.iter().map(|(r, _)| r.len()).sum();
            proptest::prop_assert_eq!(covered, frames);
            let n: usize = got.iter().map(|(_, e)| e.len()).sum();
            proptest::prop_assert_eq!(n, evs.len());
        }
    }

    /// A foreign ramp and a built-in ramp never answer for each other, even
    /// when the foreign id equals the built-in's discriminant.
    ///
    /// Mutation: compare only the numeric id in `target` (drop the `Unit`
    /// arm) → the foreign ramp with id 0 reads back as a cutoff → fails.
    #[test]
    fn foreign_and_builtin_ramps_are_distinct_addresses() {
        use tutti_types::{Hz, ParamKey};
        let foreign = ParamRamp::foreign(
            u16::from(ParamKey::<Hz>::CUTOFF.id()) as u32,
            0.5,
            Samples(0),
        );
        assert_eq!(foreign.target(ParamKey::<Hz>::CUTOFF), None);
        assert_eq!(foreign.foreign_target(0), Some(0.5));
        let builtin = ParamRamp::new(ParamKey::<Hz>::CUTOFF, Hz(100.0), Samples(0));
        assert_eq!(builtin.foreign_target(0), None);
    }

    /// The three MIDI spellings of a note-off are recognised, and a note-on
    /// or a ramp is not one.
    ///
    /// Mutation: drop the velocity-0 note-on arm → fails.
    #[test]
    fn note_offs_are_recognised() {
        let m1 = |status: u32, vel: u32| {
            Event::midi(
                Offset::ZERO,
                [0x2000_0000 | (status << 16) | (60 << 8) | vel, 0, 0, 0],
            )
        };
        assert!(m1(0x80, 64).is_note_off());
        assert!(m1(0x90, 0).is_note_off());
        assert!(!m1(0x90, 64).is_note_off());
        assert!(Event::midi(Offset::ZERO, [0x4080_3c00, 0x8000_0000, 0, 0]).is_note_off());
        assert!(!Event::midi(Offset::ZERO, [0x4090_3c00, 0x8000_0000, 0, 0]).is_note_off());
        assert!(!Event::ramp(Offset::ZERO, ParamRamp::foreign(1, 0.0, Samples(0))).is_note_off());
    }
}
