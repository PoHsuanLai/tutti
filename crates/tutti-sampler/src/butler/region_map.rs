//! Butler-thread-local map of `RegionId` → `RegionWriter`.
//!
//! Structurally a `Vec<RegionWriter>` with a `HashMap<RegionId, usize>`
//! side-index. The `Vec` is required because the parallel refill path uses
//! rayon's `par_iter_mut` (which needs `Send`, not `Sync`), and a `DashMap`
//! would force `Sync`. The index collapses the lookup pattern
//! `index.get(id).and_then(|&i| writers.get(i))` into one call.
//!
//! `RegionId`s are monotonic and never reused, so we don't compact on removal
//! today. If that ever matters, `remove` can swap_remove + reindex.

use std::collections::HashMap;

use super::command::RegionId;
use super::prefetch::RegionWriter;

pub(super) struct RegionMap {
    writers: Vec<RegionWriter>,
    index: HashMap<RegionId, usize>,
}

impl RegionMap {
    pub(super) fn new() -> Self {
        Self {
            writers: Vec::new(),
            index: HashMap::new(),
        }
    }

    pub(super) fn register(&mut self, region_id: RegionId, writer: RegionWriter) {
        let idx = self.writers.len();
        self.writers.push(writer);
        self.index.insert(region_id, idx);
    }

    pub(super) fn get(&self, region_id: RegionId) -> Option<&RegionWriter> {
        self.index
            .get(&region_id)
            .and_then(|&idx| self.writers.get(idx))
    }

    pub(super) fn get_mut(&mut self, region_id: RegionId) -> Option<&mut RegionWriter> {
        let idx = *self.index.get(&region_id)?;
        self.writers.get_mut(idx)
    }

    /// Read-only slice — used by the parallel refill path when collecting
    /// work items (before handing `writers_mut` to rayon).
    pub(super) fn writers(&self) -> &[RegionWriter] {
        &self.writers
    }

    /// Mutable slice for the parallel refill path (rayon `par_iter_mut`).
    pub(super) fn writers_mut(&mut self) -> &mut [RegionWriter] {
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

    fn make_writer(region_id: RegionId) -> RegionWriter {
        let (writer, _reader) =
            RegionBuffer::with_capacity(region_id, PathBuf::from("t.wav"), 1024);
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
        w.set_file_position(42);

        assert_eq!(reg.get(id).unwrap().file_position(), 42);
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
}
