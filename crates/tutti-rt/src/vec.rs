//! Owned, capped, slice-lending collection for the audio thread.

use arrayvec::ArrayVec;

/// A capped per-block collection that owns its storage and lends it out as
/// `&[T]`.
///
/// This is the "collect during the block, then hand the filled run to a
/// caller" shape, the most common one on the RT path.
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
/// [`is_overflowed`](Self::is_overflowed) stays set for the whole block once
/// anything has been dropped. A caller that must not lose data checks
/// `is_overflowed()` after filling and reports upward — the audio thread still
/// does not allocate either way.
///
/// `clear` resets both the contents and the flag, so the flag always
/// describes the current block.
///
/// # Examples
///
/// ```
/// use tutti_rt::RtVec;
///
/// let mut due: RtVec<u32, 2> = RtVec::new();
/// assert!(due.push(10));
/// assert!(due.push(20));
/// assert!(!due.push(30)); // refused, never grown
/// assert_eq!(due.as_slice(), &[10, 20]);
/// assert!(due.is_overflowed());
///
/// due.clear();
/// assert!(due.is_empty() && !due.is_overflowed());
/// ```
pub struct RtVec<T, const N: usize> {
    buf: ArrayVec<T, N>,
    overflowed: bool,
}

impl<T, const N: usize> RtVec<T, N> {
    /// Creates an empty collection with `N` inline slots.
    #[must_use]
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
    /// [`is_overflowed`](Self::is_overflowed)) when already at `N`.
    #[inline]
    pub fn push(&mut self, v: T) -> bool {
        if self.buf.try_push(v).is_err() {
            self.overflowed = true;
            return false;
        }
        true
    }

    /// Latches [`is_overflowed`](Self::is_overflowed) without pushing: the caller
    /// refused a group of items as a whole (one that would have to be split
    /// to fit) and dropped it.
    #[inline]
    pub fn mark_overflowed(&mut self) {
        self.overflowed = true;
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
    pub fn is_overflowed(&self) -> bool {
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
            .field("overflowed", &self.is_overflowed())
            .finish()
    }
}

#[cfg(test)]
mod tests;
