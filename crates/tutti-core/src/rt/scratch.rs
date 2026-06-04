//! Fixed-capacity scratch buffer for the audio thread.
//!
//! [`RtScratch<T>`] allocates once at construction and has no grow/resize/push
//! API, so RT code cannot reallocate through it. The active length per block is
//! chosen by slicing ([`RtScratch::active`]), not by changing the backing length.

use std::vec::Vec;
use core::fmt;

/// Fixed-capacity, no-grow scratch buffer for the audio thread.
///
/// The backing storage is allocated once in [`RtScratch::new`]; its length stays
/// equal to `capacity` for the buffer's lifetime. No public method reallocates.
///
/// Use [`try_active`](RtScratch::try_active) off the RT path (it reports an
/// overflow the caller can handle) and [`active`](RtScratch::active) /
/// [`active_ref`](RtScratch::active_ref) on the RT path (they `debug_assert` the
/// budget and clamp in release).
pub struct RtScratch<T> {
    buf: Vec<T>,
    capacity: usize,
}

/// Error from [`RtScratch::try_active`] / [`RtScratch::try_active_ref`] when the
/// requested active length exceeds the fixed capacity.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct RtScratchOverflow {
    /// Requested active length.
    pub requested: usize,
    /// Fixed capacity.
    pub capacity: usize,
}

impl fmt::Debug for RtScratchOverflow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RtScratchOverflow")
            .field("requested", &self.requested)
            .field("capacity", &self.capacity)
            .finish()
    }
}

impl fmt::Display for RtScratchOverflow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "RtScratch active length {} exceeds fixed capacity {}",
            self.requested, self.capacity
        )
    }
}

impl<T: Copy + Default> RtScratch<T> {
    /// Allocate `capacity` default-filled elements — the only allocation the
    /// type performs.
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: vec![T::default(); capacity],
            capacity,
        }
    }

    /// Fixed capacity (the maximum active length).
    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Mutable active prefix `[..len]`, or [`RtScratchOverflow`] if `len`
    /// exceeds [`capacity`](Self::capacity). For non-RT callers.
    #[inline]
    pub fn try_active(&mut self, len: usize) -> Result<&mut [T], RtScratchOverflow> {
        if len > self.capacity {
            return Err(RtScratchOverflow {
                requested: len,
                capacity: self.capacity,
            });
        }
        Ok(&mut self.buf[..len])
    }

    /// Read-only counterpart of [`try_active`](Self::try_active).
    #[inline]
    pub fn try_active_ref(&self, len: usize) -> Result<&[T], RtScratchOverflow> {
        if len > self.capacity {
            return Err(RtScratchOverflow {
                requested: len,
                capacity: self.capacity,
            });
        }
        Ok(&self.buf[..len])
    }

    /// Mutable active prefix `[..len]` for the RT hot path. `debug_assert`s
    /// `len <= capacity`; clamps to `capacity` in release. Never reallocates.
    #[inline]
    pub fn active(&mut self, len: usize) -> &mut [T] {
        debug_assert!(
            len <= self.capacity,
            "RtScratch active length ({len}) exceeds capacity ({}); sizing bug — \
             the worst case must be budgeted at construction",
            self.capacity
        );
        let len = len.min(self.capacity);
        &mut self.buf[..len]
    }

    /// Read-only counterpart of [`active`](Self::active).
    #[inline]
    pub fn active_ref(&self, len: usize) -> &[T] {
        debug_assert!(
            len <= self.capacity,
            "RtScratch active length ({len}) exceeds capacity ({}); sizing bug — \
             the worst case must be budgeted at construction",
            self.capacity
        );
        let len = len.min(self.capacity);
        &self.buf[..len]
    }
}

impl<T: Copy + Default> Clone for RtScratch<T> {
    fn clone(&self) -> Self {
        Self::new(self.capacity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_returns_correct_prefix_without_realloc() {
        let mut s = RtScratch::<f32>::new(8);
        let cap_before = s.capacity();
        let ptr_before = s.active(4).as_ptr();

        {
            let slice = s.active(4);
            assert_eq!(slice.len(), 4);
            slice.copy_from_slice(&[1.0, 2.0, 3.0, 4.0]);
        }
        assert_eq!(s.active_ref(4), &[1.0, 2.0, 3.0, 4.0]);

        // Varying the active length must not move the backing allocation.
        let _ = s.active(8);
        let _ = s.active(1);
        assert_eq!(s.active(4).as_ptr(), ptr_before, "buffer reallocated");
        assert_eq!(s.capacity(), cap_before);
    }

    #[test]
    fn try_active_ok_within_capacity() {
        let mut s = RtScratch::<f64>::new(4);
        assert_eq!(s.try_active(4).unwrap().len(), 4);
        assert_eq!(s.try_active(0).unwrap().len(), 0);
        assert!(s.try_active_ref(3).is_ok());
    }

    #[test]
    fn try_active_errors_past_capacity() {
        let mut s = RtScratch::<f32>::new(4);
        let err = s.try_active(5).unwrap_err();
        assert_eq!(err.requested, 5);
        assert_eq!(err.capacity, 4);
        assert!(s.try_active_ref(5).is_err());
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn active_clamps_in_release() {
        // Only meaningful in release: debug builds panic via debug_assert.
        let mut s = RtScratch::<f32>::new(4);
        let ptr_before = s.buf.as_ptr();
        let slice = s.active(100);
        assert_eq!(slice.len(), 4, "release-mode active must clamp to capacity");
        assert_eq!(s.buf.as_ptr(), ptr_before, "clamp must not reallocate");
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "exceeds capacity")]
    fn active_panics_in_debug_on_overflow() {
        let mut s = RtScratch::<f32>::new(4);
        let _ = s.active(5);
    }
}
