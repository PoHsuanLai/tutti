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

    /// Writes `sample` at the current write position, then advances.
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
        // `write < len` and `d < len`, so the sum is below `2 * len`: one
        // conditional subtract wraps it, where `%` would divide per tap.
        let idx = self.write + len - 1 - d;
        self.buf[if idx >= len { idx - len } else { idx }]
    }

    /// Writes `sample` over the oldest slot and returns what it held: the
    /// sample pushed `len()` pushes ago.
    ///
    /// The same as `read_back(len() - 1)` followed by [`push`](Self::push),
    /// as one slot access — the form a fixed delay of the whole ring wants.
    #[inline]
    pub fn replace_oldest(&mut self, sample: T) -> T {
        let oldest = core::mem::replace(&mut self.buf[self.write], sample);
        self.write += 1;
        if self.write >= self.buf.len() {
            self.write = 0;
        }
        oldest
    }

    /// Zeroes every slot and returns the write cursor to 0.
    ///
    /// This is what a node's `Node::reset` calls to drop the tail of the
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
///
/// The deque owns its window length, so it holds at most `window` entries
/// and never grows past the allocation [`new`](Self::new) makes: each
/// [`push`](Self::push) evicts the entry leaving the window *before* adding
/// the new one. A clone keeps that allocation.
pub struct MonotonicMinDeque {
    entries: VecDeque<(u64, f32)>,
    window: usize,
}

impl MonotonicMinDeque {
    /// Builds an empty deque over a window of `window` entries (clamped to
    /// at least 1), preallocating them.
    ///
    /// Size it to the lookahead window in samples. Allocates: call it
    /// during setup, never inside `process`.
    pub fn new(window: usize) -> Self {
        let window = window.max(1);
        Self {
            entries: VecDeque::with_capacity(window),
            window,
        }
    }

    /// The window length in entries: [`min`](Self::min) answers over the
    /// last `window` indices pushed.
    #[inline]
    pub fn window(&self) -> usize {
        self.window
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

    /// Pushes `value` stamped with `index`: first drops every entry that
    /// falls outside the window ending at `index` (stamped `index - window`
    /// or earlier), then every trailing entry that is no smaller.
    ///
    /// `index` **must increase monotonically** across calls: it is the age
    /// stamp the window is measured in, so a repeated or decreasing index
    /// makes the window boundary meaningless and the reported minimum wrong.
    ///
    /// The trailing eviction keeps the deque monotonically increasing from
    /// front to back, so the front is always the window minimum. It is O(1)
    /// amortized: each entry is pushed once and popped at most once, however
    /// many a single call discards. Evicting by age before pushing bounds
    /// the deque at `window` entries, so it never allocates and is safe on
    /// the audio thread.
    #[inline]
    pub fn push(&mut self, index: u64, value: f32) {
        let oldest = index.saturating_sub(self.window as u64 - 1);
        while let Some(&(idx, _)) = self.entries.front() {
            if idx < oldest {
                self.entries.pop_front();
            } else {
                break;
            }
        }
        while let Some(&(_, back_v)) = self.entries.back() {
            if back_v >= value {
                self.entries.pop_back();
            } else {
                break;
            }
        }
        self.entries.push_back((index, value));
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

/// A clone holds the same entries over the same window, in an allocation
/// of the full window: a derived clone would size the copy to the entries
/// it holds, and the next release would grow it on the audio thread.
impl Clone for MonotonicMinDeque {
    fn clone(&self) -> Self {
        let mut entries = VecDeque::with_capacity(self.window);
        entries.extend(self.entries.iter().copied());
        Self {
            entries,
            window: self.window,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CircularBuffer, MonotonicMinDeque};

    /// `read_back(d)` is the sample pushed `d` pushes before the newest, at
    /// every cursor position (so across every wrap), and clamps past the
    /// capacity to the oldest. Sample `k` is pushed as `k + 1`, so a slot
    /// never written reads its default `0`.
    ///
    /// Mutation (run): drop the wrap's subtract (index with `idx`) → reads
    /// past the end → panics; wrap at `idx > len` → reads index `len` →
    /// panics; subtract one more → reads the wrong tap → fails.
    #[test]
    fn read_back_counts_back_across_every_wrap() {
        const LEN: usize = 5;
        let mut ring = CircularBuffer::<f32>::new(LEN);
        for k in 0..4 * LEN {
            ring.push((k + 1) as f32);
            for d in 0..LEN + 3 {
                let back = d.min(LEN - 1);
                let want = if k >= back {
                    (k - back + 1) as f32
                } else {
                    0.0
                };
                assert_eq!(ring.read_back(d), want, "after push {k}, {d} back");
            }
        }
    }

    /// `replace_oldest` is a delay of the whole ring: it returns the sample
    /// pushed `len()` calls earlier (the default before that), and leaves
    /// the ring as a push would.
    ///
    /// Mutation (run): return the slot after writing it (`buf[write]` once
    /// replaced) → returns the input itself → fails; skip the cursor's
    /// advance → returns the previous call's input → fails.
    #[test]
    fn replace_oldest_delays_by_the_whole_ring() {
        const LEN: usize = 4;
        let mut ring = CircularBuffer::<f32>::new(LEN);
        for k in 0..5 * LEN {
            let want = if k >= LEN { (k - LEN + 1) as f32 } else { 0.0 };
            assert_eq!(ring.replace_oldest((k + 1) as f32), want, "call {k}");
            assert_eq!(ring.read_back(0), (k + 1) as f32, "newest after call {k}");
        }
    }

    /// A rising run (a limiter's release) keeps every push: the deque fills
    /// to the window and must not grow past the allocation `new` made.
    ///
    /// Mutation (run): push before evicting by age (the old order) → the
    /// deque reaches `window + 1` entries and the capacity assertion fails.
    #[test]
    fn a_rising_run_never_grows_the_deque() {
        let window = 240;
        let mut d = MonotonicMinDeque::new(window);
        let cap = d.capacity();
        for i in 0..10 * window as u64 {
            d.push(i, i as f32);
            assert_eq!(d.capacity(), cap, "grew at push {i}");
            // The window minimum of a rising run is its oldest live entry.
            let oldest = i.saturating_sub(window as u64 - 1);
            assert_eq!(d.min(), Some(oldest as f32));
        }
    }

    /// A clone of a deque that holds fewer entries than its window keeps
    /// the window's allocation, so a rising run on the clone does not grow.
    ///
    /// Mutation (run): derive `Clone` → the clone's capacity is sized to
    /// its one entry and the assertion fails.
    #[test]
    fn a_clone_keeps_the_window_allocation() {
        let window = 240;
        let mut d = MonotonicMinDeque::new(window);
        d.push(0, 1.0);
        let mut c = d.clone();
        assert!(c.capacity() >= window, "clone capacity {}", c.capacity());
        let cap = c.capacity();
        for i in 1..4 * window as u64 {
            c.push(i, i as f32);
        }
        assert_eq!(c.capacity(), cap);
        assert_eq!(c.window(), window);
    }

    /// The minimum is over exactly the last `window` pushes.
    ///
    /// Mutation (run): `oldest = index - window` (one too wide) → the
    /// fourth push still answers 0.0.
    #[test]
    fn the_minimum_spans_exactly_the_window() {
        let mut d = MonotonicMinDeque::new(3);
        d.push(0, 0.0);
        d.push(1, 5.0);
        d.push(2, 6.0);
        assert_eq!(d.min(), Some(0.0));
        d.push(3, 7.0);
        assert_eq!(d.min(), Some(5.0), "index 0 left the window of 3");
    }
}
