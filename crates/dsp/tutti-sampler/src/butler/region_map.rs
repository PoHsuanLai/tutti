//! Butler-thread-local map of `RegionId` → `RegionOut`.
//!
//! Structurally a `Vec<RegionOut>` with a `HashMap<RegionId, usize>`
//! side-index. The `Vec` is required because the parallel refill path uses
//! rayon's `par_iter_mut` (which needs `Send`, not `Sync`), and a `DashMap`
//! would force `Sync`. The index collapses the lookup pattern
//! `index.get(id).and_then(|&i| writers.get(i))` into one call.
//!
//! `RegionId`s are monotonic and never reused, so removal does not compact the
//! id space — only the `Vec`, via `swap_remove` plus a reindex of whatever moved.

use std::collections::HashMap;

use super::command::RegionId;
use super::prefetch::RegionOut;

/// The butler thread's private table of live region writers.
///
/// Not shared: the producer half of every ring lives here and nowhere else,
/// which is what lets the parallel refill hand out disjoint `&mut`s.
pub(super) struct RegionMap {
    writers: Vec<RegionOut>,
    index: HashMap<RegionId, usize>,
}

impl RegionMap {
    /// An empty map, holding no regions.
    pub(super) fn new() -> Self {
        Self {
            writers: Vec::new(),
            index: HashMap::new(),
        }
    }

    /// Take ownership of `writer` under `region_id`.
    ///
    /// Ids are minted monotonically and never reused, so this does not check for
    /// an existing entry — registering a duplicate id would orphan the old
    /// writer's index entry rather than replace it.
    pub(super) fn register(&mut self, region_id: RegionId, writer: RegionOut) {
        let idx = self.writers.len();
        self.writers.push(writer);
        self.index.insert(region_id, idx);
    }

    /// Drop the writer for `region_id`, freeing its ring buffer + `RegionMeta`.
    /// `swap_remove`s the writer and reindexes the entry that moved into its
    /// slot so the `Vec`/index side-index stays consistent. No-op if the id is
    /// unknown. Returns `true` if a writer was removed.
    pub(super) fn remove(&mut self, region_id: RegionId) -> bool {
        let Some(idx) = self.index.remove(&region_id) else {
            return false;
        };
        self.writers.swap_remove(idx);
        // `swap_remove` moved the last writer into `idx` (unless the removed one
        // WAS last); fix that writer's index entry to point at its new slot.
        if idx < self.writers.len() {
            let moved_region = self.writers[idx].region_id();
            self.index.insert(moved_region, idx);
        }
        true
    }

    /// The writer for `region_id`, or `None` if it was never registered or has
    /// been removed.
    pub(super) fn get(&self, region_id: RegionId) -> Option<&RegionOut> {
        self.index
            .get(&region_id)
            .and_then(|&idx| self.writers.get(idx))
    }

    /// Mutable access to the writer for `region_id` — the serial refill and seek
    /// paths' way in. `None` if the region is unknown.
    pub(super) fn get_mut(&mut self, region_id: RegionId) -> Option<&mut RegionOut> {
        let idx = *self.index.get(&region_id)?;
        self.writers.get_mut(idx)
    }

    /// Read-only slice (tests).
    #[cfg(test)]
    pub(super) fn writers(&self) -> &[RegionOut] {
        &self.writers
    }

    /// Mutable slice for the parallel refill path (rayon `par_iter_mut`).
    pub(super) fn writers_mut(&mut self) -> &mut [RegionOut] {
        &mut self.writers
    }

    /// Read-only view of the index — used by the parallel refill path to
    /// map `RegionId` → slice position when building work items.
    pub(super) fn index(&self) -> &HashMap<RegionId, usize> {
        &self.index
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::butler::prefetch::RegionBuffer;
    use std::path::PathBuf;

    fn make_writer(region_id: RegionId) -> RegionOut {
        let (writer, _reader) =
            RegionBuffer::with_capacity(region_id, PathBuf::from("t.wav"), 1024, 2usize);
        writer
    }

    #[test]
    fn register_then_get_roundtrip() {
        let mut reg = RegionMap::new();
        let id = RegionId(7);
        reg.register(id, make_writer(id));

        assert!(reg.get(id).is_some());
        assert!(reg.get(RegionId(999)).is_none());
    }

    #[test]
    fn get_mut_returns_same_entry() {
        let mut reg = RegionMap::new();
        let id = RegionId(1);
        reg.register(id, make_writer(id));

        let w = reg.get_mut(id).unwrap();
        w.set_play(42);

        assert_eq!(reg.get(id).unwrap().play(), 42);
    }

    #[test]
    fn writers_mut_covers_all_registered() {
        let mut reg = RegionMap::new();
        for i in 1..=3 {
            let id = RegionId(i);
            reg.register(id, make_writer(id));
        }
        assert_eq!(reg.writers_mut().len(), 3);
    }

    #[test]
    fn remove_frees_writer_and_reindexes() {
        let mut reg = RegionMap::new();
        for i in 1..=3 {
            let id = RegionId(i);
            reg.register(id, make_writer(id));
        }

        // Remove the middle one; the swapped-in survivor must stay reachable.
        assert!(reg.remove(RegionId(2)));
        assert!(reg.get(RegionId(2)).is_none());
        assert!(reg.get(RegionId(1)).is_some());
        assert!(reg.get(RegionId(3)).is_some());
        assert_eq!(reg.writers().len(), 2);

        // Removing an unknown id is a no-op.
        assert!(!reg.remove(RegionId(999)));
        assert_eq!(reg.writers().len(), 2);
    }

    /// Start + stop N streams and confirm the map is empty afterward — the
    /// regression guard for the StopStreaming ring-buffer leak.
    #[test]
    fn start_then_stop_n_streams_leaves_map_empty() {
        let mut reg = RegionMap::new();
        for i in 1..=64u64 {
            let id = RegionId(i);
            reg.register(id, make_writer(id));
            assert!(reg.remove(id));
        }
        assert_eq!(reg.writers().len(), 0);
        assert!(reg.index().is_empty());
    }
}
