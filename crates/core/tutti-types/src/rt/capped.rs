//! The capping policy shared by every fixed-capacity RT collection.
//!
//! [`Capped`] is the one place that answers "what happens at the cap?" — it
//! refuses the write, latches an overflow flag, and never grows. It is not a
//! public RT buffer on its own: it carries no access discipline, so nothing
//! about it says who may touch it or how the filled run is read back. That is
//! what the public types add, each contributing exactly one such discipline:
//!
//! - [`RtVec`](crate::RtVec) — owns one, reached through `&mut self`, lends
//!   the filled run out as `&[T]`.
//! - [`RtEventBuf`](crate::RtEventBuf) — owns one inside an
//!   [`AudioThreadCell`](crate::AudioThreadCell), so it is reachable through
//!   `&self` (behind an `Arc`, or from a COM object), and hides the storage
//!   behind visitors rather than lending it.
//!
//! Keeping the policy here is what makes those three agree by construction.
//! Before this existed the same refuse-at-`N` logic was written out three
//! times, and the engine has twice shipped a buffer that grew on the audio
//! thread because one copy of it was subtly different.

use smallvec::SmallVec;

/// A `SmallVec` that refuses to exceed its inline capacity, plus the flag
/// recording whether it has had to.
///
/// `N` is the hard ceiling: [`push`](Self::push) returns `false` rather than
/// spilling to the heap, because a spill means a `malloc` inside the audio
/// callback.
pub(crate) struct Capped<T, const N: usize> {
    buf: SmallVec<[T; N]>,
    overflowed: bool,
}

impl<T, const N: usize> Capped<T, N> {
    pub(crate) fn new() -> Self {
        Self {
            buf: SmallVec::new(),
            overflowed: false,
        }
    }

    /// Drop the contents and reset the overflow flag, keeping the storage so
    /// the next block refills into it.
    #[inline]
    pub(crate) fn clear(&mut self) {
        self.buf.clear();
        self.overflowed = false;
    }

    /// Append if there is room; otherwise drop `v`, latch the overflow flag,
    /// and report `false`.
    #[inline]
    pub(crate) fn push(&mut self, v: T) -> bool {
        if self.buf.len() >= N {
            self.overflowed = true;
            return false;
        }
        self.buf.push(v);
        true
    }

    /// Append until the cap, returning how many were written. A short return
    /// means the rest were dropped.
    #[inline]
    pub(crate) fn extend(&mut self, it: impl IntoIterator<Item = T>) -> usize {
        let mut written = 0;
        for v in it {
            if !self.push(v) {
                break;
            }
            written += 1;
        }
        written
    }

    /// Replace the contents, capped at `N`.
    #[inline]
    pub(crate) fn refill(&mut self, it: impl IntoIterator<Item = T>) -> usize {
        self.clear();
        self.extend(it)
    }

    #[inline]
    pub(crate) fn as_slice(&self) -> &[T] {
        &self.buf
    }

    #[inline]
    pub(crate) fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.buf
    }

    /// Whether anything has been dropped since the last [`clear`](Self::clear).
    #[inline]
    pub(crate) fn overflowed(&self) -> bool {
        self.overflowed
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.buf.len()
    }

    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    #[inline]
    pub(crate) fn is_full(&self) -> bool {
        self.buf.len() >= N
    }

    #[inline]
    pub(crate) fn remaining(&self) -> usize {
        N - self.buf.len()
    }

    #[inline]
    pub(crate) fn get(&self, i: usize) -> Option<&T> {
        self.buf.get(i)
    }

    #[inline]
    pub(crate) fn iter(&self) -> core::slice::Iter<'_, T> {
        self.buf.iter()
    }

    #[inline]
    pub(crate) fn sort_by_key<K: Ord>(&mut self, f: impl FnMut(&T) -> K) {
        self.buf.sort_by_key(f);
    }
}

impl<T, const N: usize> Default for Capped<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    // The policy is tested once, here. The public wrappers test only what they
    // add on top (access discipline, lending, draining) rather than re-testing
    // the cap through three different front doors.

    #[test]
    fn push_refuses_at_the_cap_rather_than_growing() {
        let mut c: Capped<u32, 3> = Capped::new();
        assert!(c.push(1));
        assert!(c.push(2));
        assert!(c.push(3));
        assert!(!c.push(4));
        assert_eq!(c.as_slice(), &[1, 2, 3]);
        assert!(c.is_full());
        assert_eq!(c.remaining(), 0);
    }

    #[test]
    fn overflow_latches_until_cleared() {
        let mut c: Capped<u32, 2> = Capped::new();
        c.push(1);
        c.push(2);
        assert!(!c.overflowed(), "full is not the same as overflowed");
        c.push(3);
        assert!(c.overflowed());
        c.clear();
        assert!(!c.overflowed(), "the flag describes the current block");
    }

    #[test]
    fn extend_and_refill_report_what_they_wrote() {
        let mut c: Capped<u32, 4> = Capped::new();
        assert_eq!(c.extend(0..2), 2);
        assert_eq!(c.extend(10..20), 2, "only the remaining room");
        assert_eq!(c.as_slice(), &[0, 1, 10, 11]);

        assert_eq!(c.refill(100..102), 2, "refill replaces, not appends");
        assert_eq!(c.as_slice(), &[100, 101]);
    }

    #[test]
    fn driving_past_the_cap_every_block_never_grows() {
        let mut c: Capped<u32, 4> = Capped::new();
        for _ in 0..1_000 {
            c.clear();
            c.extend(0..64);
            assert_eq!(c.len(), 4);
            assert_eq!(c.remaining(), 0);
        }
    }
}
