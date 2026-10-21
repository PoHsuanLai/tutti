//! [`PrerollTable`]: the per-channel pre-roll a source outside the graph
//! reads (a disk stream seeks by it).

use std::sync::{Mutex, MutexGuard, PoisonError};

use tutti_types::value::Samples;

/// The per-channel pre-roll of the plan sent last, indexed by output channel:
/// how much earlier a source outside the graph must read so its audio
/// arrives aligned. The session republishes it with every plan; the
/// sampler's butler reads it once per refill cycle, and a host may read it
/// to display.
///
/// A plain lock, not an [`ArcLend`](crate::ArcLend): **no audio thread
/// reads it.** The writer is the session's commit and the readers are the
/// butler and the host, so a reader registered per thread and a retirement
/// list would buy nothing. Each call holds the lock for one copy of the
/// table, or of one entry, and [`with`](Self::with) for its closure;
/// [`set`](Self::set) overwrites the table in place and frees nothing once
/// its capacity covers the channel count.
///
/// Share it as an `Arc<PrerollTable>`: a clone of the `Arc` reads every
/// later [`set`](Self::set).
///
/// ```
/// use tutti_core::{PrerollTable, Samples};
///
/// let table = PrerollTable::default();
/// assert_eq!(table.preroll(0), None, "empty until the first plan");
/// table.set(&[Samples(128), Samples(0)]);
/// assert_eq!(table.preroll(0), Some(Samples(128)));
/// assert_eq!(table.preroll(2), None, "past the end of the table");
/// ```
#[derive(Debug, Default)]
pub struct PrerollTable(Mutex<Vec<Samples>>);

impl PrerollTable {
    /// A table holding `channels`.
    pub fn new(channels: Vec<Samples>) -> Self {
        Self(Mutex::new(channels))
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Samples>> {
        // A poisoned lock still holds a whole table: `set` copies `Copy`
        // values, and nothing in it panics part-way.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Replaces the table with `channels`. Not for the audio thread: it
    /// locks, and allocates when the table grows.
    pub fn set(&self, channels: &[Samples]) {
        let mut table = self.lock();
        table.clear();
        table.extend_from_slice(channels);
    }

    /// `channel`'s pre-roll, or `None` past the end of the table (no
    /// compensation needed).
    pub fn preroll(&self, channel: usize) -> Option<Samples> {
        self.lock().get(channel).copied()
    }

    /// The whole table, copied.
    pub fn to_vec(&self) -> Vec<Samples> {
        self.lock().clone()
    }

    /// `f` over the table under one lock: a consistent view for a pass over
    /// several channels. Keep `f` short; [`set`](Self::set) waits on it.
    pub fn with<R>(&self, f: impl FnOnce(&[Samples]) -> R) -> R {
        f(&self.lock())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A `set` replaces the whole table**: a shorter table leaves no
    /// entry of the longer one behind, and every reader sees the last `set`.
    ///
    /// Mutation (run): `set` not clearing first (appending) → channel 0
    /// still reads 200 and the table keeps four entries, fails.
    #[test]
    fn a_set_replaces_the_whole_table() {
        let table = PrerollTable::new(vec![Samples(200), Samples(0), Samples(100)]);
        assert_eq!(table.preroll(2), Some(Samples(100)));
        table.set(&[Samples(7)]);
        assert_eq!(table.preroll(0), Some(Samples(7)));
        assert_eq!(table.preroll(1), None, "the old entry is gone");
        assert_eq!(table.to_vec(), [Samples(7)]);
        assert_eq!(table.with(<[Samples]>::len), 1);
    }
}
