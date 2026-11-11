//! [`PrerollTable`]: a shared copy of `Plan::compensation()` for a host that
//! displays it. Nothing in this crate uses it; it lives here because the
//! session that writes it and a host that reads it both depend on this crate.

use std::sync::{Mutex, MutexGuard, PoisonError};

use tutti_types::value::Samples;

/// The per-channel compensation of the plan sent last, indexed by output
/// channel: how much the compiler delays that output to align it with the
/// latest. The session republishes it with every plan, and a host may read it
/// to display. A source inside the graph needs none of it: the compiler
/// aligns it.
///
/// A plain lock, not an [`ArcLend`](tutti_rt::ArcLend): **no audio thread
/// reads it.** The writer is the session's commit and the reader is a
/// host, so a reader registered per thread and a retirement
/// list would buy nothing. Each call holds the lock for one copy of the
/// table, or of one entry, and [`with`](Self::with) for its closure;
/// [`set`](Self::set) overwrites the table in place and frees nothing once
/// its capacity covers the channel count.
///
/// Share it as an `Arc<PrerollTable>`: a clone of the `Arc` reads every
/// later [`set`](Self::set).
///
/// ```
/// use tutti_graph::PrerollTable;
/// use tutti_types::Samples;
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
    #[must_use]
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
