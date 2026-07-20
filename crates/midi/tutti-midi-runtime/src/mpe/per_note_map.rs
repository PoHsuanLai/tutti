//! Lock-free, allocation-free per-note atomic store keyed by [`NoteId`].
//!
//! This is the RT-safe, atomic sibling of [`tutti_midi_types::PerNoteMap`]. The
//! `PerNoteMap` in `tutti-midi-types` is `T: Copy + Default` and lives behind an
//! `&mut` — it cannot hold atomics and is for single-threaded callers (the synth
//! allocator). [`AtomicPerNoteMap`] instead stores its values in atomics so it can
//! be shared across the audio thread and the MIDI thread through an `Arc`, with no
//! locks and no allocation after construction.
//!
//! **Keyed by the full [`NoteId`]**, not by note number: two live notes that share
//! a note number (same pitch, different channel, or a rotation-minted id) occupy
//! *distinct* slots and stay independent — the storage half of MIDI 2.0 per-note
//! addressing.
//!
//! Capacity is fixed at `N` slots. A slot is claimed on [`insert`](AtomicPerNoteMap::insert)
//! /[`entry`](AtomicPerNoteMap::entry) and released on [`remove`](AtomicPerNoteMap::remove);
//! lookup is a linear probe over the id column. All operations are `&self` and use
//! acquire/release ordering, so a reader on the audio thread sees a consistent
//! `(id, value)` pairing.

use core::sync::atomic::{AtomicU32, Ordering};

use tutti_midi_types::NoteId;

/// A per-note value column made of atomics, one entry per map slot.
///
/// Implementors expose the atomic fields for one note. The map owns the id/occupancy
/// bookkeeping; the slot type owns only the payload and how to clear it to a default.
pub trait AtomicSlot {
    /// Construct one default slot (called `N` times at map construction).
    fn new_default() -> Self;
    /// Reset this slot's value(s) to their defaults without touching occupancy.
    fn clear(&self);
}

/// `NoteId`'s reserved "empty slot" sentinel. `NoteId::from_channel_note` only ever
/// sets the low 12 bits, and `from_raw` users avoid this value, so `u32::MAX` is a
/// safe vacancy marker.
const EMPTY: u32 = u32::MAX;

/// Fixed-capacity, lock-free, allocation-free per-note store keyed by [`NoteId`].
///
/// `S` is the per-note atomic payload; `N` is the maximum number of simultaneously
/// live notes. Sharing is via `Arc<AtomicPerNoteMap<S, N>>`; every method takes
/// `&self`.
pub struct AtomicPerNoteMap<S, const N: usize> {
    ids: [AtomicU32; N],
    slots: [S; N],
}

impl<S: AtomicSlot, const N: usize> Default for AtomicPerNoteMap<S, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: AtomicSlot, const N: usize> AtomicPerNoteMap<S, N> {
    pub fn new() -> Self {
        Self {
            ids: core::array::from_fn(|_| AtomicU32::new(EMPTY)),
            slots: core::array::from_fn(|_| S::new_default()),
        }
    }

    /// Linear-probe for the slot currently holding `id`. The [`EMPTY`] sentinel id
    /// never matches an occupied slot (it is the vacancy marker), so a caller that
    /// passes it gets `None` rather than aliasing a free slot.
    #[inline]
    fn find(&self, id: NoteId) -> Option<usize> {
        let raw = id.raw();
        if raw == EMPTY {
            return None;
        }
        self.ids
            .iter()
            .position(|slot| slot.load(Ordering::Acquire) == raw)
    }

    /// Claim (or find) a slot for `id`, resetting it to defaults on first claim.
    /// Returns the slot payload, or `None` only when the map is full and `id` is new
    /// (or `id` is the reserved [`EMPTY`] sentinel, which is not storable).
    #[inline]
    pub fn entry(&self, id: NoteId) -> Option<&S> {
        if id.raw() == EMPTY {
            return None;
        }
        if let Some(i) = self.find(id) {
            return Some(&self.slots[i]);
        }
        let raw = id.raw();
        // Claim the first vacant slot. Single-writer on the control thread, so a
        // plain find-then-store is sufficient; the store publishes with Release.
        let i = self
            .ids
            .iter()
            .position(|slot| slot.load(Ordering::Acquire) == EMPTY)?;
        self.slots[i].clear();
        self.ids[i].store(raw, Ordering::Release);
        Some(&self.slots[i])
    }

    /// The payload for `id`, if a slot is currently claimed for it.
    #[inline]
    pub fn get(&self, id: NoteId) -> Option<&S> {
        self.find(id).map(|i| &self.slots[i])
    }

    /// Release `id`'s slot (freeing capacity). The payload is left as-is until the
    /// slot is next claimed, at which point [`entry`](Self::entry) clears it.
    #[inline]
    pub fn remove(&self, id: NoteId) {
        if let Some(i) = self.find(id) {
            self.ids[i].store(EMPTY, Ordering::Release);
        }
    }

    /// Free every slot and clear every payload.
    #[inline]
    pub fn clear_all(&self) {
        for i in 0..N {
            self.ids[i].store(EMPTY, Ordering::Release);
            self.slots[i].clear();
        }
    }

    /// Iterate the payloads of currently-claimed slots, paired with their `NoteId`.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = (NoteId, &S)> {
        self.ids.iter().zip(self.slots.iter()).filter_map(|(id, s)| {
            let raw = id.load(Ordering::Acquire);
            (raw != EMPTY).then(|| (NoteId::from_raw(raw), s))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_float::AtomicF32;
    use core::sync::atomic::AtomicBool;

    /// A minimal slot used to exercise the map's id bookkeeping.
    struct TestSlot {
        value: AtomicF32,
        active: AtomicBool,
    }

    impl AtomicSlot for TestSlot {
        fn new_default() -> Self {
            Self {
                value: AtomicF32::new(0.0),
                active: AtomicBool::new(false),
            }
        }
        fn clear(&self) {
            self.value.store(0.0, Ordering::Release);
            self.active.store(false, Ordering::Release);
        }
    }

    #[test]
    fn same_note_number_distinct_ids_are_independent() {
        // Two ids sharing a note number must not alias in the store.
        let map: AtomicPerNoteMap<TestSlot, 8> = AtomicPerNoteMap::new();
        let a = NoteId::from_channel_note(0, 60);
        let b = NoteId::from_channel_note(1, 60); // same pitch, different channel
        assert_ne!(a.raw(), b.raw());

        map.entry(a).unwrap().value.store(0.25, Ordering::Release);
        map.entry(b).unwrap().value.store(0.75, Ordering::Release);

        assert_eq!(map.get(a).unwrap().value.load(Ordering::Acquire), 0.25);
        assert_eq!(map.get(b).unwrap().value.load(Ordering::Acquire), 0.75);
    }

    #[test]
    fn remove_frees_capacity_without_disturbing_others() {
        let map: AtomicPerNoteMap<TestSlot, 2> = AtomicPerNoteMap::new();
        let a = NoteId::from_channel_note(0, 60);
        let b = NoteId::from_channel_note(0, 64);
        map.entry(a).unwrap().value.store(1.0, Ordering::Release);
        map.entry(b).unwrap().value.store(2.0, Ordering::Release);

        map.remove(a);
        assert!(map.get(a).is_none());
        assert_eq!(map.get(b).unwrap().value.load(Ordering::Acquire), 2.0);

        // The freed slot is reusable, and a fresh claim clears stale payload.
        let c = NoteId::from_channel_note(0, 67);
        let slot = map.entry(c).unwrap();
        assert_eq!(slot.value.load(Ordering::Acquire), 0.0);
    }

    #[test]
    fn full_map_rejects_new_ids_but_keeps_serving_present_ones() {
        let map: AtomicPerNoteMap<TestSlot, 2> = AtomicPerNoteMap::new();
        let a = NoteId::from_raw(1);
        let b = NoteId::from_raw(2);
        let c = NoteId::from_raw(3);
        assert!(map.entry(a).is_some());
        assert!(map.entry(b).is_some());
        assert!(map.entry(c).is_none()); // full — no blocking, just None
        // existing id still resolves when full
        assert!(map.entry(a).is_some());
    }

    #[test]
    fn entry_clears_on_first_claim_only() {
        let map: AtomicPerNoteMap<TestSlot, 4> = AtomicPerNoteMap::new();
        let a = NoteId::from_channel_note(0, 60);
        map.entry(a).unwrap().value.store(0.9, Ordering::Release);
        // Re-entry of a live id must NOT wipe the stored value.
        assert_eq!(map.entry(a).unwrap().value.load(Ordering::Acquire), 0.9);
    }
}
