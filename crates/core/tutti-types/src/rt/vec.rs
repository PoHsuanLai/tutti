//! Owned, capped, slice-lending collection for the audio thread.

use arrayvec::ArrayVec;

/// A capped per-block collection that owns its storage and lends it out as
/// `&[T]`.
///
/// This is the "collect during the block, then hand the filled run to a
/// caller" shape — the most common one on the RT path, and the one the other
/// `rt` buffers do not serve:
///
/// A caller that needs the same buffer through `&self` (behind an `Arc`, in a
/// COM object) wraps it in an [`AudioThreadCell`](crate::AudioThreadCell).
///
/// # The cap is the point
///
/// `push` **refuses** at `N` and returns `false`; it never grows. A bare
/// `Vec` in this position grows instead, which means a `malloc`
/// inside the audio callback (and, if drained with `mem::take`, a `free` there
/// too). The storage is an inline [`ArrayVec`], which has no heap path at all,
/// so the collection itself never touches the heap. (Its items may: `clear`
/// drops them where it runs, so an `RtVec<Box<_>>` cleared on the audio
/// thread frees there.)
///
/// # Overflow is visible, not silent
///
/// Dropping events is a real behaviour change, so it is reportable rather
/// than quiet: [`push`](Self::push) returns whether the item was stored,
/// [`extend`](Self::extend) returns how many it wrote, and
/// [`overflowed`](Self::overflowed) stays set for the whole block once
/// anything has been dropped. A caller that must not lose data checks
/// `overflowed()` after filling and reports upward — the audio thread still
/// does not allocate either way.
///
/// `clear` resets both the contents and the flag, so the flag always
/// describes the current block.
///
/// # Examples
///
/// ```
/// use tutti_types::RtVec;
///
/// let mut due: RtVec<u32, 2> = RtVec::new();
/// assert!(due.push(10));
/// assert!(due.push(20));
/// assert!(!due.push(30)); // refused, never grown
/// assert_eq!(due.as_slice(), &[10, 20]);
/// assert!(due.overflowed());
///
/// due.clear();
/// assert!(due.is_empty() && !due.overflowed());
/// ```
pub struct RtVec<T, const N: usize> {
    buf: ArrayVec<T, N>,
    overflowed: bool,
}

impl<T, const N: usize> RtVec<T, N> {
    /// Creates an empty collection with `N` inline slots.
    pub fn new() -> Self {
        Self {
            buf: ArrayVec::new(),
            overflowed: false,
        }
    }

    /// Drops all items and resets the overflow flag, keeping the storage.
    #[inline]
    pub fn clear(&mut self) {
        self.buf.clear();
        self.overflowed = false;
    }

    /// Appends one item if there is room.
    ///
    /// Returns `false` (dropping `v`, and latching
    /// [`overflowed`](Self::overflowed)) when already at `N`.
    #[inline]
    pub fn push(&mut self, v: T) -> bool {
        if self.buf.try_push(v).is_err() {
            self.overflowed = true;
            return false;
        }
        true
    }

    /// Appends from an iterator, stopping at the cap.
    ///
    /// Returns how many items were written; a short return means the rest were
    /// dropped.
    #[inline]
    pub fn extend(&mut self, it: impl IntoIterator<Item = T>) -> usize {
        let mut written = 0;
        for v in it {
            if !self.push(v) {
                break;
            }
            written += 1;
        }
        written
    }

    /// Returns the filled run.
    #[inline]
    pub fn as_slice(&self) -> &[T] {
        &self.buf
    }

    /// Returns the filled run mutably, for in-place fixups (sorting by frame
    /// offset, clamping) that do not change the length.
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.buf
    }

    /// Returns whether anything has been dropped since the last
    /// [`clear`](Self::clear).
    #[inline]
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// Returns how many items are in the filled run; never more than `N`.
    #[inline]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Returns whether nothing has been pushed since the last
    /// [`clear`](Self::clear).
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Returns whether the next `push` will be refused.
    #[inline]
    pub fn is_full(&self) -> bool {
        self.buf.is_full()
    }

    /// Returns the room left before the cap.
    #[inline]
    pub fn remaining(&self) -> usize {
        self.buf.remaining_capacity()
    }

    /// Returns the cap, `N`, for code where `N` is not in scope.
    #[inline]
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Iterates over the filled run.
    #[inline]
    pub fn iter(&self) -> core::slice::Iter<'_, T> {
        self.buf.iter()
    }
}

impl<T, const N: usize> Default for RtVec<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> core::ops::Deref for RtVec<T, N> {
    type Target = [T];
    #[inline]
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<'a, T, const N: usize> IntoIterator for &'a RtVec<T, N> {
    type Item = &'a T;
    type IntoIter = core::slice::Iter<'a, T>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.buf.iter()
    }
}

impl<T: core::fmt::Debug, const N: usize> core::fmt::Debug for RtVec<T, N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RtVec")
            .field("items", &self.as_slice())
            .field("cap", &N)
            .field("overflowed", &self.overflowed())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    /// Mutation (run): `push` through `ArrayVec::push` instead of `try_push`
    /// → the fourth push panics ("ArrayVec: capacity exceeded") → fails.
    #[test]
    fn push_fills_then_refuses_at_the_cap() {
        let mut v: RtVec<u32, 3> = RtVec::new();
        assert!(v.push(1));
        assert!(v.push(2));
        assert!(v.push(3));
        assert!(!v.push(4), "push past N must be refused");
        assert_eq!(v.as_slice(), &[1, 2, 3]);
        assert_eq!(v.len(), 3);
        assert!(v.is_full());
        assert_eq!(v.remaining(), 0);
    }

    #[test]
    fn overflow_is_reported_and_latches_for_the_block() {
        let mut v: RtVec<u32, 2> = RtVec::new();
        assert!(!v.overflowed());
        v.push(1);
        v.push(2);
        assert!(!v.overflowed(), "at capacity but nothing dropped yet");
        v.push(3);
        assert!(v.overflowed(), "a refused push latches the flag");
        assert!(v.overflowed(), "and it stays set for the rest of the block");
    }

    #[test]
    fn clear_resets_contents_and_the_flag() {
        let mut v: RtVec<u32, 2> = RtVec::new();
        v.extend(0..10);
        assert!(v.overflowed());
        v.clear();
        assert!(v.is_empty());
        assert!(
            !v.overflowed(),
            "the flag describes the current block, not history"
        );
    }

    #[test]
    fn extend_reports_how_many_it_wrote() {
        let mut v: RtVec<u32, 4> = RtVec::new();
        assert_eq!(v.extend(0..2), 2, "everything fits");
        assert!(!v.overflowed());
        assert_eq!(v.extend(100..200), 2, "only the remaining room");
        assert!(v.overflowed());
        assert_eq!(v.as_slice(), &[0, 1, 100, 101]);
    }

    #[test]
    fn as_slice_serves_a_borrowed_return() {
        // The shape the plugin-host pools need: fill, then hand `&[T]` back.
        fn lend(v: &RtVec<u32, 8>) -> &[u32] {
            v.as_slice()
        }
        let mut v: RtVec<u32, 8> = RtVec::new();
        v.extend([7, 8, 9]);
        assert_eq!(lend(&v), &[7, 8, 9]);
    }

    #[test]
    fn as_mut_slice_allows_in_place_fixups() {
        let mut v: RtVec<u32, 8> = RtVec::new();
        v.extend([3, 1, 2]);
        v.as_mut_slice().sort_unstable();
        assert_eq!(v.as_slice(), &[1, 2, 3]);
    }

    #[test]
    fn deref_and_iter_read_the_filled_run_only() {
        let mut v: RtVec<u32, 8> = RtVec::new();
        v.extend([4, 5]);
        assert_eq!(v.len(), 2, "not the inline capacity");
        assert_eq!(v.first(), Some(&4), "slice methods via Deref");
        let seen: Vec<u32> = v.iter().copied().collect();
        assert_eq!(seen, std::vec![4, 5]);
    }

    #[test]
    fn refilling_past_the_cap_every_block_stays_at_the_cap() {
        // The regression this type exists to prevent: driven past `N` on every
        // block, the collection must never grow. Checked via `remaining`,
        // which `no_std` can assert without a heap probe.
        let mut v: RtVec<u32, 4> = RtVec::new();
        for _ in 0..1_000 {
            v.clear();
            v.extend(0..64);
            assert_eq!(v.len(), 4);
            assert_eq!(v.remaining(), 0);
            assert!(v.overflowed());
        }
    }
}
