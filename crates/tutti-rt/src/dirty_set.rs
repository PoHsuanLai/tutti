//! [`DirtySet`]: a fixed-size set of dirty flags, one atomic bit per index,
//! that the audio thread marks for a control thread to take.
//!
//! A producer that has more to say than a queue can hold (a parameter
//! automated on every block, a value changed many times between two control
//! ticks) should not push each change: it marks that the thing changed, and
//! the consumer reads the latest state itself. A bit is a level, not a count,
//! so marking an index twice is one take, and there is nothing to overflow:
//! the set has a bit for every index it was built for.
//!
//! The set is sized once, from a count, on the control side
//! ([`DirtySet::new`] allocates); after that no call allocates, locks or
//! waits. An index is a [`DirtyIndex`] that only the set mints
//! ([`DirtySet::index`] refuses one at or past [`len`](DirtySet::len)), so
//! [`set`](DirtySet::set) takes only an index in range and nothing is masked
//! or dropped. A `DirtyIndex` belongs to the set that minted it; handing it to
//! a set of another size is a programming error (an out-of-range one panics).
//!
//! # The protocol
//!
//! The bits live in `u64` words.
//!
//! - **Set:** `fetch_or(bit, Release)` on the index's word. Wait-free.
//! - **Take:** `swap(0, Acquire)` on each word in turn, yielding the bits it
//!   returned. A bit set before the swap in the word's modification order is
//!   yielded by this take or an earlier one, and a bit set after it stays for
//!   the next. None is lost and none is yielded twice, because the swap and
//!   the `fetch_or` are both read-modify-writes of one location. The words
//!   are swapped one at a time, as the iterator reaches them, so a take is a
//!   set of per-word snapshots, not one snapshot of the whole set; a bit set
//!   while a take is under way is in this take or the next, which is all a
//!   dirty flag promises. A take dropped before it ends has not touched the
//!   words it did not reach, so those bits wait for the next.
//!
//! The `Release`/`Acquire` pair orders what the setter wrote before marking
//! (the state the flag stands for) before what the taker reads after taking.
//! There is no store-then-load pair on either side, so no `SeqCst` fence is
//! involved (the same shape as [`RequestWord`](crate::RequestWord), whose
//! word is one `u32` of named requests; this set is sized at construction).
//!
//! # Who may call
//!
//! Both calls take `&self`, and any number of threads may set and any number
//! may take: every bit goes to exactly one take whoever calls. The intended
//! shape is one setter (the audio thread) and one taker (the control
//! thread), but nothing breaks if it is not, so the set is shared as an
//! `Arc<DirtySet>` rather than split into a `!Clone` writer and reader as
//! [`edit_ring`](crate::edit_ring) is: that split guards a slot whose
//! hand-over needs exactly one writer and one reader, and a word of flags has
//! no slot to guard.
//!
//! The model in `tests/dirty_set_loom.rs` runs this code.

use sync::{AtomicU64, Ordering};

mod sync {
    #[cfg(loom)]
    pub(super) use loom::sync::atomic::{AtomicU64, Ordering};
    #[cfg(not(loom))]
    pub(super) use std::sync::atomic::{AtomicU64, Ordering};
}

const WORD_BITS: usize = u64::BITS as usize;

/// One index of a [`DirtySet`], minted by [`DirtySet::index`] (or yielded by a
/// take), so it is in range of the set it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DirtyIndex(usize);

impl DirtyIndex {
    /// The position.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }
}

impl From<DirtyIndex> for usize {
    fn from(index: DirtyIndex) -> usize {
        index.0
    }
}

/// A fixed-size set of dirty flags, one atomic bit per index, that the audio
/// thread marks for a control thread to take.
///
/// ```
/// use tutti_rt::{DirtyIndex, DirtySet};
///
/// let set = DirtySet::new(100);
/// let seven = set.index(7).unwrap();
/// set.set(seven);
/// set.set(seven);
/// set.set(set.index(70).unwrap());
/// assert!(set.index(100).is_none(), "past the end");
/// let taken: Vec<usize> = set.take().map(DirtyIndex::get).collect();
/// assert_eq!(taken, [7, 70], "coalesced, lowest first");
/// assert_eq!(set.take().count(), 0, "taken once");
/// ```
#[derive(Debug)]
pub struct DirtySet {
    len: usize,
    words: Box<[AtomicU64]>,
}

impl DirtySet {
    /// A set of `len` flags, all clear. Allocates: the control side's call.
    #[must_use]
    pub fn new(len: usize) -> Self {
        let words = len.div_ceil(WORD_BITS);
        Self {
            len,
            words: (0..words).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    /// How many flags the set has.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the set has no flags.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The flag at `index`, or `None` if it is [`len`](Self::len) or more.
    #[must_use]
    pub fn index(&self, index: usize) -> Option<DirtyIndex> {
        (index < self.len).then_some(DirtyIndex(index))
    }

    /// Marks `index`. Any thread; wait-free; never allocates or locks.
    ///
    /// # Panics
    ///
    /// If `index` came from a larger set (it is not one of this set's).
    pub fn set(&self, index: DirtyIndex) {
        self.words[index.0 / WORD_BITS].fetch_or(1 << (index.0 % WORD_BITS), Ordering::Release);
    }

    /// The indices marked since they were last taken, lowest first, clearing
    /// each word as the iterator reaches it. Allocation-free.
    ///
    /// A set bit is yielded by exactly one take. Dropping the iterator early
    /// leaves the words it did not reach marked for the next take.
    #[must_use = "the iterator is the take: it clears each word as it reaches it"]
    pub fn take(&self) -> Taken<'_> {
        Taken {
            words: self.words.iter().enumerate(),
            base: 0,
            bits: 0,
        }
    }
}

/// The indices one [`DirtySet::take`] yields.
#[derive(Debug)]
pub struct Taken<'a> {
    words: std::iter::Enumerate<std::slice::Iter<'a, AtomicU64>>,
    base: usize,
    bits: u64,
}

impl Iterator for Taken<'_> {
    type Item = DirtyIndex;

    fn next(&mut self) -> Option<DirtyIndex> {
        while self.bits == 0 {
            let (word, atomic) = self.words.next()?;
            self.base = word * WORD_BITS;
            self.bits = atomic.swap(0, Ordering::Acquire);
        }
        let bit = self.bits.trailing_zeros() as usize;
        self.bits &= self.bits - 1;
        Some(DirtyIndex(self.base + bit))
    }
}

#[cfg(all(test, not(loom)))]
mod tests;
