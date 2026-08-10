//! Shared ring-buffer primitives: `CircularBuffer` and `MonotonicMinDeque`.
//!
//! `CircularBuffer` centralizes the write-index + wrap logic the delay,
//! modulation and limiter modules would otherwise each hand-roll.
//! `MonotonicMinDeque` powers the lookahead-limiter's O(1) sliding-window
//! minimum.

use std::collections::VecDeque;

/// Fixed-capacity ring buffer with explicit write position.
///
/// `len()` returns the backing storage length (capacity). `push` overwrites
/// the oldest sample; `get(delay)` reads `delay` samples back from the most
/// recent write.
#[derive(Clone)]
pub struct CircularBuffer<T: Copy + Default> {
    buf: Vec<T>,
    write: usize,
}

impl<T: Copy + Default> CircularBuffer<T> {
    /// Allocates a ring of `capacity` slots, zeroed, with the write cursor at 0.
    ///
    /// `capacity` is clamped to at least 1, so a degenerate request yields a
    /// one-slot ring rather than a buffer whose every index panics. Allocates —
    /// build it before the node goes live, never inside `process`.
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: vec![T::default(); capacity.max(1)],
            write: 0,
        }
    }

    /// Returns the backing storage length — the ring's fixed capacity, not a
    /// count of samples written.
    ///
    /// This never grows and never reads back as 0; the ring is full of
    /// `T::default()` from construction.
    #[inline]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Returns whether the backing storage is empty, which it never is —
    /// [`new`](Self::new) clamps capacity to at least 1.
    ///
    /// Present because `clippy::len_without_is_empty` asks for it alongside
    /// [`len`](Self::len).
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Returns the index the next [`push`](Self::push) will write to.
    ///
    /// Callers doing their own fractional-sample index math against
    /// [`as_slice`](Self::as_slice) need this as the origin to count back from.
    #[inline]
    pub fn write_pos(&self) -> usize {
        self.write
    }

    /// Write `sample` at the current write position, then advance.
    #[inline]
    pub fn push(&mut self, sample: T) {
        self.buf[self.write] = sample;
        self.write += 1;
        if self.write >= self.buf.len() {
            self.write = 0;
        }
    }

    /// Reads by **absolute ring index**, wrapping modulo capacity — not
    /// relative to the write cursor.
    ///
    /// This is the raw addressing form, for callers that track their own read
    /// position (a fractional-delay reader interpolating between two taps). To
    /// read relative to the newest sample, use [`read_back`](Self::read_back);
    /// confusing the two reads an arbitrary point in the ring rather than the
    /// intended tap, which sounds like a wrong or wandering delay time.
    #[inline]
    pub fn at(&self, index_from_start: usize) -> T {
        self.buf[index_from_start % self.buf.len()]
    }

    /// Reads the sample written `delay_samples` pushes ago, counting back from
    /// the most recent write.
    ///
    /// `delay_samples = 0` is the newest sample. The delay is clamped to
    /// `capacity - 1`, so an over-long request returns the oldest sample still
    /// held rather than panicking — a delay node asking past its allocation
    /// shortens silently instead of failing.
    #[inline]
    pub fn read_back(&self, delay_samples: usize) -> T {
        let len = self.buf.len();
        let d = delay_samples.min(len - 1);
        let idx = (self.write + len - 1 - d) % len;
        self.buf[idx]
    }

    /// Zeroes every slot and returns the write cursor to 0.
    ///
    /// This is what a node's `AudioUnit::reset` calls to drop the tail of the
    /// previous take; without it a restarted delay bleeds the old signal. Does
    /// not reallocate, so it is safe on the audio thread.
    pub fn clear(&mut self) {
        for s in self.buf.iter_mut() {
            *s = T::default();
        }
        self.write = 0;
    }

    /// Direct access to the backing slice (e.g. for fractional-sample reads
    /// that need their own index math).
    #[inline]
    pub fn as_slice(&self) -> &[T] {
        &self.buf
    }
}

/// Sliding-window minimum via a monotonic deque. O(1) amortized per push.
///
/// Used by the lookahead limiter to track the smallest gain (greatest
/// reduction) over the lookahead window. Stores `(age_index, value)` pairs
/// so stale entries can be evicted by age.
#[derive(Clone)]
pub struct MonotonicMinDeque {
    entries: VecDeque<(u64, f32)>,
}

impl MonotonicMinDeque {
    /// Builds an empty deque, preallocating `capacity_hint` entries.
    ///
    /// Size the hint to the lookahead window in samples: the deque never holds
    /// more than one entry per sample in flight, so a hint that covers the
    /// window keeps [`push`](Self::push) from reallocating on the audio thread.
    /// Allocates — call it during setup, never inside `process`.
    pub fn new(capacity_hint: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity_hint),
        }
    }

    /// Drops every entry, leaving the window empty and
    /// [`min`](Self::min) answering `None`.
    ///
    /// Retains the allocation, so it is safe on the audio thread.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Capacity of the underlying deque (for footprint accounting).
    #[inline]
    pub fn capacity(&self) -> usize {
        self.entries.capacity()
    }

    /// Pushes `value` stamped with `index`, evicting every trailing entry that
    /// is no smaller.
    ///
    /// `index` **must increase monotonically** across calls — it is the age
    /// stamp [`evict_older_than`](Self::evict_older_than) compares against, so a
    /// repeated or decreasing index makes the window boundary meaningless and
    /// the reported minimum wrong.
    ///
    /// The eviction is what keeps the deque monotonically increasing from front
    /// to back, so the front is always the window minimum. It is O(1)
    /// amortized: each entry is pushed once and popped at most once, however
    /// many a single call discards. Never allocates beyond the capacity hint, so
    /// it is safe on the audio thread.
    #[inline]
    pub fn push(&mut self, index: u64, value: f32) {
        while let Some(&(_, back_v)) = self.entries.back() {
            if back_v >= value {
                self.entries.pop_back();
            } else {
                break;
            }
        }
        self.entries.push_back((index, value));
    }

    /// Drop entries older than `min_index`.
    #[inline]
    pub fn evict_older_than(&mut self, min_index: u64) {
        while let Some(&(idx, _)) = self.entries.front() {
            if idx < min_index {
                self.entries.pop_front();
            } else {
                break;
            }
        }
    }

    /// Current window minimum. Returns `None` when the deque is empty.
    #[inline]
    pub fn min(&self) -> Option<f32> {
        self.entries.front().map(|&(_, v)| v)
    }

    /// Returns whether the window holds no entries, in which case
    /// [`min`](Self::min) answers `None`.
    ///
    /// True before the first [`push`](Self::push) and after
    /// [`clear`](Self::clear); a limiter reads it as "no gain reduction is in
    /// flight yet".
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
