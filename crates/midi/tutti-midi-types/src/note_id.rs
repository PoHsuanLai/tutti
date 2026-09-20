//! Per-note identity for MIDI 2.0 per-note addressing.
//!
//! MIDI 2.0 has no note-index field on the wire — per-note messages still key on
//! (channel, note number). [`NoteId`] is the *host-internal* identity that lets
//! two simultaneous notes sharing a note number stay independent (the point of
//! per-note addressing). On the MIDI 1.0 / classic-MPE path it is a pure function
//! of (channel, note), so behaviour is identical to keying on the raw pair; under
//! Note Number Rotation an allocator mints distinct ids via [`NoteId::from_raw`].

use tutti_types::MidiChannel;

/// Stable per-note identity.
///
/// The default (MIDI-1) encoding packs `channel` and `note` so
/// [`NoteId::note_number`] / [`NoteId::channel`] recover them; rotation-minted
/// ids are opaque and need not carry either.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct NoteId(u32);

impl From<u32> for NoteId {
    /// Same as [`NoteId::from_raw`] — treat a raw value as an opaque id.
    #[inline]
    fn from(v: u32) -> Self {
        Self(v)
    }
}

impl From<NoteId> for u32 {
    /// The opaque backing value (see [`NoteId::raw`]).
    #[inline]
    fn from(id: NoteId) -> Self {
        id.0
    }
}

impl NoteId {
    /// MIDI-1 / classic-MPE identity: `(channel, note)`.
    ///
    /// `note` is masked to its 7-bit MIDI width so the packed value is
    /// self-consistent with [`note_number`](Self::note_number) — a `note`
    /// ≥ 128 can never alias a different pair or read back changed.
    /// [`MidiChannel`] already carries the 4-bit guarantee for its half.
    #[inline]
    pub const fn from_channel_note(channel: MidiChannel, note: u8) -> Self {
        Self(((channel.get() as u32) << 8) | (note & 0x7f) as u32)
    }

    /// Note-Number-Rotation identity: an allocator-minted distinct value.
    #[inline]
    pub const fn from_raw(v: u32) -> Self {
        Self(v)
    }

    /// The MIDI note number, for the [`from_channel_note`](Self::from_channel_note) encoding.
    #[inline]
    pub const fn note_number(self) -> u8 {
        (self.0 & 0x7f) as u8
    }

    /// The MIDI channel, for the [`from_channel_note`](Self::from_channel_note) encoding.
    #[inline]
    pub const fn channel(self) -> MidiChannel {
        MidiChannel::new(((self.0 >> 8) & 0x0f) as u8)
    }

    /// The opaque backing value.
    #[inline]
    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// Fixed-capacity, allocation-free per-note store keyed by full [`NoteId`].
///
/// Keyed by the whole id (not note number) so two same-pitch notes never
/// collide — the storage half of per-note addressing. Audio-thread safe: no
/// allocation, only a linear probe over `N` slots. `N` is the max simultaneous
/// notes (e.g. 128); [`insert`](Self::insert) returns `false` when full.
#[derive(Clone)]
pub struct PerNoteMap<T, const N: usize> {
    ids: [Option<NoteId>; N],
    vals: [T; N],
}

impl<T: Copy + Default, const N: usize> Default for PerNoteMap<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Copy + Default, const N: usize> PerNoteMap<T, N> {
    /// Builds an empty map with capacity for `N` simultaneous notes.
    ///
    /// Allocation-free — the storage is inline arrays, which is what makes this
    /// usable on the audio thread.
    #[inline]
    pub fn new() -> Self {
        Self {
            ids: [None; N],
            vals: [T::default(); N],
        }
    }

    #[inline]
    fn slot(&self, id: NoteId) -> Option<usize> {
        self.ids.iter().position(|&s| s == Some(id))
    }

    /// Get a mutable handle to `id`'s value, inserting a default one if absent.
    /// Returns `None` only when the map is full and `id` is not present.
    #[inline]
    fn entry(&mut self, id: NoteId) -> Option<&mut T> {
        let slot = self
            .slot(id)
            .or_else(|| self.ids.iter().position(Option::is_none))?;
        self.ids[slot] = Some(id);
        Some(&mut self.vals[slot])
    }

    /// The value stored for `id`, or `None` if the note has no entry.
    ///
    /// A linear scan of at most `N` slots — cheap for the small `N` a voice map
    /// uses, and the reason this is not a hash map.
    #[inline]
    pub fn get(&self, id: NoteId) -> Option<&T> {
        self.slot(id).map(|s| &self.vals[s])
    }

    /// Insert or overwrite. Returns `false` if the map is full and `id` is new.
    #[inline]
    pub fn insert(&mut self, id: NoteId, value: T) -> bool {
        match self.entry(id) {
            Some(v) => {
                *v = value;
                true
            }
            None => false,
        }
    }

    /// Remove `id`, returning its value if present.
    #[inline]
    pub fn remove(&mut self, id: NoteId) -> Option<T> {
        let slot = self.slot(id)?;
        self.ids[slot] = None;
        Some(core::mem::take(&mut self.vals[slot]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_note_roundtrip() {
        let id = NoteId::from_channel_note(MidiChannel::new(9), 60);
        assert_eq!(id.channel(), MidiChannel::new(9));
        assert_eq!(id.note_number(), 60);
    }

    #[test]
    fn same_note_number_distinct_ids_do_not_collide() {
        // The whole point: two live notes on note 60 must stay independent.
        let a = NoteId::from_channel_note(MidiChannel::new(0), 60);
        let b = NoteId::from_raw(0xDEAD_0000); // rotation-minted, same sounding pitch
        assert_ne!(a, b);

        let mut map: PerNoteMap<i32, 8> = PerNoteMap::new();
        assert!(map.insert(a, 1));
        assert!(map.insert(b, 2));
        assert_eq!(map.get(a), Some(&1));
        assert_eq!(map.get(b), Some(&2));
        assert_eq!(map.remove(a), Some(1));
        assert_eq!(map.get(b), Some(&2));
    }

    #[test]
    fn full_map_rejects_new_ids() {
        let mut map: PerNoteMap<u8, 2> = PerNoteMap::new();
        assert!(map.insert(NoteId::from_raw(1), 1));
        assert!(map.insert(NoteId::from_raw(2), 2));
        assert!(!map.insert(NoteId::from_raw(3), 3));
        // existing key still writable when full
        assert!(map.insert(NoteId::from_raw(1), 9));
        assert_eq!(map.get(NoteId::from_raw(1)), Some(&9));
    }

    /// An over-wide note number is masked into its 7 bits, never carried into
    /// the channel field.
    ///
    /// The doc on [`NoteId::from_channel_note`] promises a `note` ≥ 128 "can
    /// never alias a different pair or read back changed". Without the mask the
    /// 8th bit lands in the channel nibble, so note 128 on channel 0 would be
    /// indistinguishable from note 0 on channel 1 — two live notes silently
    /// sharing one id.
    #[test]
    fn an_over_wide_note_is_masked_rather_than_aliased() {
        for note in [128u8, 129, 200, 255] {
            let id = NoteId::from_channel_note(MidiChannel::new(0), note);
            assert_eq!(
                id.channel(),
                MidiChannel::new(0),
                "note {note} moved channel"
            );
            assert_eq!(
                id.note_number(),
                note & 0x7f,
                "note {note} read back changed"
            );
            assert_ne!(
                id,
                NoteId::from_channel_note(MidiChannel::new(1), note & 0x7f),
                "note {note} aliased onto the next channel"
            );
        }
    }

    #[test]
    fn ord_makes_it_a_btree_key() {
        use std::collections::BTreeMap;
        let mut m = BTreeMap::new();
        m.insert(NoteId::from_channel_note(MidiChannel::new(0), 64), "b");
        m.insert(NoteId::from_channel_note(MidiChannel::new(0), 60), "a");
        // BTreeMap requires Ord; keys come back sorted.
        let notes: Vec<u8> = m.keys().map(|k| k.note_number()).collect();
        assert_eq!(notes, [60, 64]);
    }
}
