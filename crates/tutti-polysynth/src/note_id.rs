//! The per-note store keyed by [`NoteId`], which `tutti-ump` owns.

use tutti_ump::NoteId;

/// Fixed-capacity, allocation-free per-note store keyed by full [`NoteId`].
///
/// Keyed by the whole id (not note number) so two same-pitch notes never
/// collide — the storage half of per-note addressing. Audio-thread safe: no
/// allocation, only a linear probe over `N` slots. `N` is the max simultaneous
/// notes (e.g. 128); [`insert`](Self::insert) returns `false` when full.
#[derive(Clone, Debug)]
pub(crate) struct PerNoteMap<T, const N: usize> {
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
    #[must_use]
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

    /// Inserts or overwrites. Returns `false` if the map is full and `id` is new.
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

    /// Removes `id`, returning its value if present.
    #[inline]
    pub fn remove(&mut self, id: NoteId) -> Option<T> {
        let slot = self.slot(id)?;
        self.ids[slot] = None;
        Some(core::mem::take(&mut self.vals[slot]))
    }
}

#[cfg(test)]
mod tests;
