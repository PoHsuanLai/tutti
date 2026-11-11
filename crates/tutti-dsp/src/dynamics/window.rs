//! The lookahead limiter's two windows: the fixed delay each channel's audio
//! takes, and the minimum of the gains over the lookahead.

use std::collections::VecDeque;

/// A fixed delay of `len` frames: each sample written comes back `len`
/// writes later, zeros before that. One allocation, made by
/// [`new`](Self::new).
#[derive(Clone, Debug)]
pub(crate) struct FixedDelay {
    buf: Vec<f32>,
    write: usize,
}

impl FixedDelay {
    /// A delay of `len` frames (at least 1), holding silence. Allocates.
    pub(crate) fn new(len: usize) -> Self {
        Self {
            buf: vec![0.0; len.max(1)],
            write: 0,
        }
    }

    /// Writes `x` and returns the sample written `len` calls earlier.
    #[inline]
    pub(crate) fn step(&mut self, x: f32) -> f32 {
        let out = core::mem::replace(&mut self.buf[self.write], x);
        self.write += 1;
        if self.write == self.buf.len() {
            self.write = 0;
        }
        out
    }

    /// Back to silence, without reallocating.
    pub(crate) fn clear(&mut self) {
        self.buf.fill(0.0);
        self.write = 0;
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
pub(crate) struct MonotonicMinDeque {
    entries: VecDeque<(u64, f32)>,
    window: usize,
}

impl std::fmt::Debug for MonotonicMinDeque {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MonotonicMinDeque")
            .field("len", &self.entries.len())
            .field("window", &self.window)
            .finish_non_exhaustive()
    }
}

impl MonotonicMinDeque {
    /// Builds an empty deque over a window of `window` entries (clamped to
    /// at least 1), preallocating them.
    ///
    /// Size it to the lookahead window in samples. Allocates: call it
    /// during setup, never inside `process`.
    #[must_use]
    pub(crate) fn new(window: usize) -> Self {
        let window = window.max(1);
        Self {
            entries: VecDeque::with_capacity(window),
            window,
        }
    }

    /// Drops every entry, leaving the window empty and
    /// [`min`](Self::min) answering `None`.
    ///
    /// Retains the allocation, so it is safe on the audio thread.
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
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
    pub(crate) fn push(&mut self, index: u64, value: f32) {
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
    pub(crate) fn min(&self) -> Option<f32> {
        self.entries.front().map(|&(_, v)| v)
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
    use super::{FixedDelay, MonotonicMinDeque};

    /// A delay of `len` returns each sample `len` calls later, and silence
    /// before the first one arrives, across every wrap and after a clear.
    #[test]
    fn a_fixed_delay_returns_each_sample_len_calls_later() {
        const LEN: usize = 4;
        let mut d = FixedDelay::new(LEN);
        for round in 0..2 {
            for k in 0..5 * LEN {
                let want = if k >= LEN { (k - LEN + 1) as f32 } else { 0.0 };
                assert_eq!(d.step((k + 1) as f32), want, "round {round}, call {k}");
            }
            d.clear();
        }
    }

    /// A rising run (a limiter's release) keeps every push: the deque fills
    /// to the window and must not grow past the allocation `new` made.
    #[test]
    fn a_rising_run_never_grows_the_deque() {
        let window = 240;
        let mut d = MonotonicMinDeque::new(window);
        let cap = d.entries.capacity();
        for i in 0..10 * window as u64 {
            d.push(i, i as f32);
            assert_eq!(d.entries.capacity(), cap, "grew at push {i}");
            // The window minimum of a rising run is its oldest live entry.
            let oldest = i.saturating_sub(window as u64 - 1);
            assert_eq!(d.min(), Some(oldest as f32));
        }
    }

    /// A clone of a deque that holds fewer entries than its window keeps
    /// the window's allocation, so a rising run on the clone does not grow.
    #[test]
    fn a_clone_keeps_the_window_allocation() {
        let window = 240;
        let mut d = MonotonicMinDeque::new(window);
        d.push(0, 1.0);
        let mut c = d.clone();
        assert!(
            c.entries.capacity() >= window,
            "clone capacity {}",
            c.entries.capacity()
        );
        let cap = c.entries.capacity();
        for i in 1..4 * window as u64 {
            c.push(i, i as f32);
        }
        assert_eq!(c.entries.capacity(), cap);
        assert_eq!(c.window, window);
    }

    /// The minimum is over exactly the last `window` pushes.
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
