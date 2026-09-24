//! Events: what flows on an event port.
//!
//! Doc 013 §1 ("Port kinds") and owner decision 4: events are **graph ports**,
//! not a side channel, so the one compiler pass that aligns audio (PDC) aligns
//! notes and automation too. Decision 6: an event input port may have several
//! sources, merged deterministically by `(offset, source order)` — layering a
//! keyboard and a clip is the normal case, not an edge case. Decision 7:
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

use tutti_types::{ParamKey, Samples, Unit, UnitParam};

/// Four raw Universal MIDI Packet words.
///
/// The meaningful prefix is determined by the message type in the first word's
/// top nibble, exactly as `tutti_midi_types::MidiEvent::data_words` reads it;
/// unused words are zero. The graph never inspects them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Ump(pub [u32; 4]);

/// A linear ramp of one parameter to a target, starting at the event's offset.
///
/// Built from a typed [`ParamKey<U>`] and a `U`, and read back the same way,
/// so the unit is checked at both ends. In between it travels type-erased —
/// the key's stable [`UnitParam`] id plus the raw `f32` — because one event
/// port carries ramps for parameters of different units, and an `Event` has
/// to be one `Copy` type. The raw value is private: nothing reads it without
/// naming the key, and so the unit.
///
/// ```
/// use tutti_graph::ParamRamp;
/// use tutti_types::{Db, Hz, ParamKey, Samples};
/// let r = ParamRamp::new(ParamKey::<Hz>::CUTOFF, Hz(800.0), Samples(64));
/// assert_eq!(r.target(ParamKey::<Hz>::CUTOFF), Some(Hz(800.0)));
/// assert_eq!(r.target(ParamKey::<Db>::THRESHOLD), None); // another param
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParamRamp {
    param: UnitParam,
    target: f32,
    duration: Samples,
}

impl ParamRamp {
    /// Ramp `key` to `target` over `duration` (zero is a step).
    pub fn new<U: Unit<Raw = f32>>(key: ParamKey<U>, target: U, duration: Samples) -> Self {
        Self {
            param: key.id(),
            target: target.to_raw(),
            duration,
        }
    }

    /// Which parameter this ramps, untyped — for routing on the id.
    pub fn param(&self) -> UnitParam {
        self.param
    }

    /// The target, in `key`'s unit, if this ramp is for `key`.
    pub fn target<U: Unit<Raw = f32>>(&self, key: ParamKey<U>) -> Option<U> {
        (self.param == key.id()).then(|| U::from_raw(self.target))
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

/// One event on an event port: a frame offset into the current block, and what
/// happens there.
///
/// `offset` is frames from the start of the block the event is delivered in —
/// always `< block length`. Slices of events handed to a node are sorted by it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Event {
    /// Frames from the start of the block.
    pub offset: u32,
    /// The payload.
    pub kind: EventKind,
}

impl Event {
    /// A MIDI event at `offset`.
    pub const fn midi(offset: u32, words: [u32; 4]) -> Self {
        Self {
            offset,
            kind: EventKind::Midi(Ump(words)),
        }
    }

    /// A parameter ramp starting at `offset`.
    pub const fn ramp(offset: u32, ramp: ParamRamp) -> Self {
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
            if e.offset as usize >= frames {
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
        if let Some(at) = events.iter().position(|e| e.offset as usize >= frames) {
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
        if event.offset >= self.frames {
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
        let mut best: Option<(u32, usize)> = None;
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
        Event::midi(offset, [tag, 0, 0, 0])
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
}
