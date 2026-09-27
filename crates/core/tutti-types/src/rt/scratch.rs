//! Fixed-capacity scratch buffer for the audio thread.
//!
//! [`RtScratch<T>`] allocates once at construction and has no grow/resize/push
//! API, so RT code cannot reallocate through it. The active length per block is
//! chosen by slicing ([`RtScratch::active`]), not by changing the backing length.

use core::fmt;
use std::vec::Vec;

/// A fixed-capacity scratch buffer for the audio thread that can never grow.
///
/// The backing storage is allocated once in [`RtScratch::new`]; its length stays
/// equal to `capacity` for the buffer's lifetime. There is no grow, resize or
/// push API, so RT code cannot reallocate through it: the active length per
/// block is chosen by slicing a prefix, not by changing the backing length.
///
/// Use [`try_active`](RtScratch::try_active) off the RT path (it reports an
/// overflow the caller can handle) and [`active`](RtScratch::active) /
/// [`active_ref`](RtScratch::active_ref) on the RT path (they `debug_assert` the
/// budget and clamp in release).
///
/// `Clone` allocates a fresh default-filled buffer of the same capacity; it
/// does not copy the contents.
///
/// # Examples
///
/// ```
/// use tutti_types::RtScratch;
///
/// let mut scratch = RtScratch::<f32>::new(512);
/// let block = scratch.active(128);
/// block.fill(0.25);
/// assert_eq!(scratch.active_ref(128)[0], 0.25);
/// assert!(scratch.try_active(1024).is_err());
/// ```
pub struct RtScratch<T> {
    buf: Vec<T>,
    capacity: usize,
}

/// The error [`RtScratch::try_active`] and [`RtScratch::try_active_ref`] return
/// when the requested active length exceeds the fixed capacity.
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
    /// Allocates `capacity` default-filled elements.
    ///
    /// This is the only allocation the type performs, so call it off the audio
    /// thread.
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: vec![T::default(); capacity],
            capacity,
        }
    }

    /// Returns the fixed capacity (the maximum active length).
    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns the mutable active prefix `[..len]`.
    ///
    /// For callers that can handle a sizing error.
    ///
    /// # Errors
    ///
    /// [`RtScratchOverflow`] if `len` exceeds [`capacity`](Self::capacity).
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

    /// Returns the read-only active prefix `[..len]`; see
    /// [`try_active`](Self::try_active).
    ///
    /// # Errors
    ///
    /// [`RtScratchOverflow`] if `len` exceeds [`capacity`](Self::capacity).
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

    /// Returns the mutable active prefix `[..len]`, for the RT hot path.
    ///
    /// Never reallocates. In release builds a `len` past the capacity is
    /// clamped to it.
    ///
    /// # Panics
    ///
    /// In debug builds, if `len` exceeds [`capacity`](Self::capacity): the
    /// worst case must be budgeted at construction.
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

    /// Returns the read-only active prefix `[..len]`; see
    /// [`active`](Self::active).
    ///
    /// # Panics
    ///
    /// In debug builds, if `len` exceeds [`capacity`](Self::capacity).
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
