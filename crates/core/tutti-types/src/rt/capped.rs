//! The capping policy shared by every fixed-capacity RT collection.
//!
//! [`Capped`] is the one place that answers "what happens at the cap?" — it
//! refuses the write, latches an overflow flag, and never grows. It is not a
//! public RT buffer on its own: it carries no access discipline, so nothing
//! about it says who may touch it or how the filled run is read back. That is
//! what [`RtVec`](crate::RtVec) adds: it owns one, is reached through
//! `&mut self`, and lends the filled run out as `&[T]`. A caller that needs
//! the same buffer through `&self` wraps an `RtVec` in an
//! [`AudioThreadCell`](crate::AudioThreadCell).

use arrayvec::ArrayVec;

/// `N` inline slots that refuse a write past them, plus the flag recording
/// whether one has been refused.
///
/// `N` is the hard ceiling: [`push`](Self::push) returns `false` rather than
/// growing. The storage is an [`ArrayVec`], which has no heap path at all, so
/// there is no spill to guard against: a `malloc` inside the audio callback
/// is not reachable from here.
pub(crate) struct Capped<T, const N: usize> {
    buf: ArrayVec<T, N>,
    overflowed: bool,
}

impl<T, const N: usize> Capped<T, N> {
    pub(crate) fn new() -> Self {
        Self {
            buf: ArrayVec::new(),
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
        if self.buf.try_push(v).is_err() {
            self.overflowed = true;
            return false;
        }
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
        self.buf.is_full()
    }

    #[inline]
    pub(crate) fn remaining(&self) -> usize {
        self.buf.remaining_capacity()
    }

    #[inline]
    pub(crate) fn iter(&self) -> core::slice::Iter<'_, T> {
        self.buf.iter()
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

    // The policy is tested once, here. `RtVec` tests only what it adds on top
    // (access discipline, lending) rather than re-testing the cap.

    /// Mutation (run): `push` through `ArrayVec::push` instead of `try_push`
    /// → the fourth push panics ("ArrayVec: capacity exceeded") → fails.
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
    fn extend_reports_what_it_wrote() {
        let mut c: Capped<u32, 4> = Capped::new();
        assert_eq!(c.extend(0..2), 2);
        assert_eq!(c.extend(10..20), 2, "only the remaining room");
        assert_eq!(c.as_slice(), &[0, 1, 10, 11]);
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
