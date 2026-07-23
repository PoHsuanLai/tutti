//! Fixed-inline-capacity event collector for the audio thread.

use super::cell::AudioThreadCell;
use smallvec::SmallVec;

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
pub struct RtEventBuf<T, const N: usize> {
    cell: AudioThreadCell<SmallVec<[T; N]>>,
}

impl<T, const N: usize> RtEventBuf<T, N> {
    /// An empty collector with `N` inline slots.
    pub fn new() -> Self {
        Self {
            cell: AudioThreadCell::new(SmallVec::new()),
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
        let mut buf = self.cell.borrow_mut();
        buf.clear();
        for v in it {
            if buf.len() >= N {
                break;
            }
            buf.push(v);
        }
    }

    /// Append one event if there is room. Returns `false` (dropping `v`) when
    /// the collector is already at capacity `N` — this is what keeps the push
    /// allocation-free on the audio thread.
    #[inline]
    pub fn push(&self, v: T) -> bool {
        let mut buf = self.cell.borrow_mut();
        if buf.len() >= N {
            return false;
        }
        buf.push(v);
        true
    }

    /// Visit each event in order. RT-safe; no early-stop — callers that want
    /// to skip a range do so per-element.
    #[inline]
    pub fn for_each(&self, mut f: impl FnMut(&T)) {
        for v in self.cell.borrow().iter() {
            f(v);
        }
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
    fn clear_empties() {
        let buf: RtEventBuf<u32, 4> = RtEventBuf::new();
        buf.refill([1, 2]);
        buf.clear();
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);
    }
}
