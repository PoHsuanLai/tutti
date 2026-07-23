//! Shared ring-buffer primitives: `CircularBuffer` and `MonotonicMinDeque`.
//!
//! `CircularBuffer` centralizes the write-index + wrap logic that the delay,
//! modulation, and limiter modules previously hand-rolled. `MonotonicMinDeque`
//! powers the lookahead-limiter's O(1) sliding-window minimum.

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
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: vec![T::default(); capacity.max(1)],
            write: 0,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

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

    /// Read the sample written `delay_samples` writes ago (clamped to
    /// `capacity - 1`). `delay_samples = 0` would return the just-written
    /// sample; callers that treat "just written" as a fresh read should pass
    /// `len - 1 - delay` as appropriate — see `read_from_newest`.
    #[inline]
    pub fn at(&self, index_from_start: usize) -> T {
        self.buf[index_from_start % self.buf.len()]
    }

    /// Read `delay_samples` samples before the write cursor. Clamped to
    /// `capacity - 1`.
    #[inline]
    pub fn read_back(&self, delay_samples: usize) -> T {
        let len = self.buf.len();
        let d = delay_samples.min(len - 1);
        let idx = (self.write + len - 1 - d) % len;
        self.buf[idx]
    }

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
    pub fn new(capacity_hint: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity_hint),
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Capacity of the underlying deque (for footprint accounting).
    #[inline]
    pub fn capacity(&self) -> usize {
        self.entries.capacity()
    }

    /// Push a new value at `index` (monotonically increasing index).
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

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

