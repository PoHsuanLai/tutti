//! Fixed-inline-capacity event collector for the audio thread.

use super::capped::Capped;
use super::cell::AudioThreadCell;

/// An RT-safe event collector: a `SmallVec` with `N` inline slots behind an
/// [`AudioThreadCell`], so it can be filled and read through `&self` on the
/// audio thread without a lock and without reallocating.
///
/// Length is bundled with the data (it lives in the `SmallVec`), so producer
/// and consumer never disagree about how many events are live.
///
/// The collector is **capped at `N`**: [`push`](Self::push) refuses to grow
/// past the inline capacity (returning `false`) rather than spilling to the
/// heap — overflow on the audio thread is dropped, never allocated. Size the
/// `N` to the worst case at construction.
impl<T, const N: usize> core::fmt::Debug for RtEventBuf<T, N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Capacity only: the contents sit behind an `AudioThreadCell`, and
        // reaching through it here would take a borrow the audio thread may
        // already hold.
        f.debug_struct("RtEventBuf")
            .field("capacity", &N)
            .finish_non_exhaustive()
    }
}

pub struct RtEventBuf<T, const N: usize> {
    cell: AudioThreadCell<Capped<T, N>>,
}

impl<T, const N: usize> RtEventBuf<T, N> {
    /// An empty collector with `N` inline slots.
    pub fn new() -> Self {
        Self {
            cell: AudioThreadCell::new(Capped::new()),
        }
    }

    /// Drop all events.
    #[inline]
    pub fn clear(&self) {
        self.cell.borrow_mut().clear();
    }

    /// Replace the contents with `it`, capped at `N`. Items past the inline
    /// capacity are dropped (never spilled to the heap).
    #[inline]
    pub fn refill(&self, it: impl IntoIterator<Item = T>) {
        self.cell.borrow_mut().refill(it);
    }

    /// Append one event if there is room. Returns `false` (dropping `v`) when
    /// the collector is already at capacity `N` — this is what keeps the push
    /// allocation-free on the audio thread.
    #[inline]
    pub fn push(&self, v: T) -> bool {
        self.cell.borrow_mut().push(v)
    }

    /// Whether anything has been dropped since the last
    /// [`clear`](Self::clear) or [`refill`](Self::refill).
    #[inline]
    pub fn overflowed(&self) -> bool {
        self.cell.borrow().overflowed()
    }

    /// Visit each event in order. RT-safe; no early-stop — callers that want
    /// to skip a range do so per-element.
    #[inline]
    pub fn for_each(&self, mut f: impl FnMut(&T)) {
        for v in self.cell.borrow().iter() {
            f(v);
        }
    }

    /// Take each event in order, leaving the collector empty, **without
    /// freeing the backing buffer**.
    ///
    /// This is the shape [`for_each`](Self::for_each) cannot serve: `for_each`
    /// holds a borrow on the cell for its whole traversal, so the callback
    /// cannot touch the structure the events refer back to. `drain_each`
    /// releases the borrow around every call, so `f` may take `&mut` to
    /// whatever owns this collector — the "collect indices this block, then
    /// consume them while mutating the collection they index" pattern.
    ///
    /// Prefer this over `core::mem::take` on a bare `SmallVec`: `take` swaps
    /// in a fresh buffer and drops the outgoing one, which frees on the audio
    /// thread if the collection ever spilled, and re-allocates the next block.
    /// Here the buffer is retained and only its length is reset.
    ///
    /// `f` is called at most `N` times, in push order, and the collector is
    /// empty when it returns.
    ///
    /// Requires `T: Copy`: each item is copied out before the cell borrow is
    /// released, which is what lets `f` reach back into the owner. Every RT
    /// event type in the engine is `Copy` (indices, `MidiEvent`, parameter
    /// points); a non-`Copy` payload does not belong on this path.
    #[inline]
    pub fn drain_each(&self, mut f: impl FnMut(T))
    where
        T: Copy,
    {
        let mut i = 0;
        loop {
            // Re-borrow per item rather than holding across the callback, so
            // `f` is free to reach back into the owner of this collector.
            // Indexing forwards (rather than `pop`) keeps the order the events
            // were pushed in, which is what callers of an event buffer expect.
            let v = {
                let buf = self.cell.borrow();
                match buf.get(i) {
                    Some(v) => *v,
                    None => break,
                }
            };
            f(v);
            i += 1;
        }
        self.cell.borrow_mut().clear();
    }

    /// Sort the active events in place by a derived key. Stable across calls;
    /// does not expose the backing storage.
    #[inline]
    pub fn sort_by_key<K: Ord>(&self, f: impl FnMut(&T) -> K) {
        self.cell.borrow_mut().sort_by_key(f);
    }

    /// Number of live events.
    #[inline]
    pub fn len(&self) -> usize {
        self.cell.borrow().len()
    }

    /// Whether there are no live events.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.cell.borrow().is_empty()
    }

    /// Reset the audio-thread owner (delegates to the inner cell — currently a
    /// no-op, kept for source compatibility).
    #[inline]
    pub fn reset_owner(&self) {
        self.cell.reset_owner();
    }
}

impl<T, const N: usize> Default for RtEventBuf<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    #[test]
    fn refill_then_read_in_order() {
        let buf: RtEventBuf<u32, 8> = RtEventBuf::new();
        buf.refill([3, 1, 2]);
        assert_eq!(buf.len(), 3);

        let mut seen = Vec::new();
        buf.for_each(|&v| seen.push(v));
        assert_eq!(seen, std::vec![3, 1, 2]);
    }

    #[test]
    fn push_caps_at_n_and_drops_overflow() {
        let buf: RtEventBuf<u32, 2> = RtEventBuf::new();
        assert!(buf.push(10));
        assert!(buf.push(20));
        assert!(!buf.push(30), "third push must be dropped at capacity 2");
        assert_eq!(buf.len(), 2);

        let mut seen = Vec::new();
        buf.for_each(|&v| seen.push(v));
        assert_eq!(seen, std::vec![10, 20]);
    }

    #[test]
    fn refill_caps_at_n() {
        let buf: RtEventBuf<u32, 3> = RtEventBuf::new();
        buf.refill(0..100);
        assert_eq!(buf.len(), 3);
    }

    #[test]
    fn sort_by_key_orders_active_region() {
        let buf: RtEventBuf<(u32, char), 8> = RtEventBuf::new();
        buf.refill([(3, 'c'), (1, 'a'), (2, 'b')]);
        buf.sort_by_key(|&(k, _)| k);

        let mut seen = Vec::new();
        buf.for_each(|&(_, c)| seen.push(c));
        assert_eq!(seen, std::vec!['a', 'b', 'c']);
    }

    #[test]
    fn drain_each_visits_in_push_order_and_empties() {
        let buf: RtEventBuf<u32, 8> = RtEventBuf::new();
        buf.refill([10, 20, 30]);

        let mut seen = Vec::new();
        buf.drain_each(|v| seen.push(v));

        assert_eq!(seen, std::vec![10, 20, 30], "push order, not reversed");
        assert!(buf.is_empty(), "collector is empty after draining");
    }

    #[test]
    fn drain_each_on_empty_calls_nothing() {
        let buf: RtEventBuf<u32, 4> = RtEventBuf::new();
        let mut calls = 0;
        buf.drain_each(|_| calls += 1);
        assert_eq!(calls, 0);
    }

    #[test]
    fn drain_each_callback_may_reborrow_the_buffer() {
        // The reason `drain_each` exists: `for_each` holds the cell borrow
        // across the whole traversal, so a callback that touches the same
        // collector would panic on the debug in-use flag. `drain_each`
        // releases the borrow around each call.
        let buf: RtEventBuf<u32, 8> = RtEventBuf::new();
        buf.refill([1, 2, 3]);

        let mut seen = Vec::new();
        buf.drain_each(|v| {
            // Re-entrant read of the same collector from inside the callback.
            let _still_readable = buf.len();
            seen.push(v);
        });

        assert_eq!(seen, std::vec![1, 2, 3]);
        assert!(buf.is_empty());
    }

    #[test]
    fn drain_each_then_refill_reuses_the_buffer() {
        let buf: RtEventBuf<u32, 4> = RtEventBuf::new();
        for _ in 0..100 {
            buf.refill([1, 2, 3, 4]);
            let mut n = 0;
            buf.drain_each(|_| n += 1);
            assert_eq!(n, 4);
            assert!(buf.is_empty());
        }
    }

    #[test]
    fn clear_empties() {
        let buf: RtEventBuf<u32, 4> = RtEventBuf::new();
        buf.refill([1, 2]);
        buf.clear();
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);
    }
}
