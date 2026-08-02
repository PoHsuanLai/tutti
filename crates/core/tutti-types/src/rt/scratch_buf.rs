//! Fill-then-lend scratch buffer for the audio thread.

use core::cell::UnsafeCell;
use smallvec::SmallVec;

/// An RT-safe scratch buffer that is refilled each block and then **lends its
/// filled contents out as a `&self`-lifetime slice**.
///
/// This is the one capability [`AudioThreadCell`](crate::AudioThreadCell) (and
/// thus [`RtEventBuf`](crate::RtEventBuf), built on it) intentionally does not
/// provide: their scoped borrow guards only expose the data for the guard's
/// lifetime, so a borrow can never outlive the method that took it. Some RT
/// APIs must hand a reference *back to the caller* — e.g. a trait whose
/// contract is "returns a slice valid until the next call". `RtScratchBuf`
/// serves exactly that shape, and confines the `unsafe` it requires to this
/// one type.
///
/// Backed by a `SmallVec` with `N` inline slots, so refilling never reallocates
/// once warmed and the type stays `no_std`. Like [`RtEventBuf`], it is **capped
/// at `N`**: items past the inline capacity are dropped rather than spilled to
/// the heap. The cap is structural — [`fill_and_read`](Self::fill_and_read)
/// hands the closure a [`CappedWriter`], not the backing `SmallVec`, so there
/// is no `push` that can grow past `N`.
///
/// # Safety contract
///
/// All access must come from a single thread at a time (the audio callback).
/// Unlike `AudioThreadCell` there is no debug in-use flag, because the returned
/// slice deliberately outlives any internal guard — so the single-thread
/// invariant is the caller's to uphold, exactly as it was for the raw
/// `UnsafeCell` this replaces.
///
/// [`RtEventBuf`]: crate::RtEventBuf
pub struct RtScratchBuf<T, const N: usize> {
    inner: UnsafeCell<SmallVec<[T; N]>>,
}

/// Write handle handed to [`RtScratchBuf::fill_and_read`]'s closure: the only
/// way to put items into the buffer, and it refuses to exceed the inline
/// capacity.
///
/// This exists so the "capped at `N`" guarantee is enforced by the type rather
/// than by caller discipline. Exposing the backing `SmallVec` would hand the
/// closure a `push` that silently heap-allocates on the audio thread the
/// moment it runs one past `N` — the same reason [`RtEventBuf`]'s `push`
/// returns `bool` instead of growing.
///
/// [`RtEventBuf`]: crate::RtEventBuf
pub struct CappedWriter<'a, T, const N: usize> {
    buf: &'a mut SmallVec<[T; N]>,
}

impl<T, const N: usize> CappedWriter<'_, T, N> {
    /// Append one item if there is room. Returns `false` (dropping `v`) when
    /// the buffer already holds `N` items.
    #[inline]
    pub fn push(&mut self, v: T) -> bool {
        if self.buf.len() >= N {
            return false;
        }
        self.buf.push(v);
        true
    }

    /// Append from an iterator, stopping at capacity. Returns the number of
    /// items actually written, so a caller can tell whether input was dropped.
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

    /// Number of items written so far this fill.
    #[inline]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether nothing has been written yet this fill.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Whether the buffer has reached its cap — further pushes will be dropped.
    #[inline]
    pub fn is_full(&self) -> bool {
        self.buf.len() >= N
    }

    /// Remaining room before the cap.
    #[inline]
    pub fn remaining(&self) -> usize {
        N - self.buf.len()
    }
}

impl<T, const N: usize> RtScratchBuf<T, N> {
    /// An empty buffer with `N` inline slots.
    pub fn new() -> Self {
        Self {
            inner: UnsafeCell::new(SmallVec::new()),
        }
    }

    /// Clear the buffer, run `fill` to repopulate it, then return the filled
    /// contents as a slice borrowing `&self` (valid until the next call that
    /// touches the buffer).
    ///
    /// `fill` receives a [`CappedWriter`] over the cleared buffer and pushes
    /// the block's items into it. Writes past `N` are **dropped**, not spilled
    /// to the heap — `CappedWriter::push` returns `false` at capacity — so
    /// size `N` to the worst case if dropping is not acceptable.
    ///
    /// # Safety
    /// The caller must guarantee single-audio-thread access: no other thread
    /// may call this (or otherwise touch the buffer) while either this call is
    /// running or the returned slice is still alive.
    #[inline]
    pub unsafe fn fill_and_read(&self, fill: impl FnOnce(&mut CappedWriter<'_, T, N>)) -> &[T] {
        // SAFETY: single-audio-thread access per the method contract; the
        // mutable borrow ends before the shared reborrow below.
        let buf = unsafe { &mut *self.inner.get() };
        buf.clear();
        fill(&mut CappedWriter { buf });
        // SAFETY: as above; the returned slice borrows `self`, which is the
        // whole reason this type wraps `UnsafeCell` rather than building on
        // `AudioThreadCell`'s scoped guards.
        unsafe { (*self.inner.get()).as_slice() }
    }
}

impl<T, const N: usize> Default for RtScratchBuf<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: `RtScratchBuf<T>` is Send/Sync if `T` is Send. The buffer is reached
// through `&self` (typically behind an `Arc`); soundness rests on the
// single-audio-thread access contract documented on `fill_and_read`, exactly
// as it does for `AudioThreadCell`.
unsafe impl<T: Send, const N: usize> Send for RtScratchBuf<T, N> {}
unsafe impl<T: Send, const N: usize> Sync for RtScratchBuf<T, N> {}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    #[test]
    fn fill_and_read_returns_filled_slice() {
        let buf: RtScratchBuf<u32, 8> = RtScratchBuf::new();
        let slice = unsafe {
            buf.fill_and_read(|v| {
                v.extend([1, 2, 3]);
            })
        };
        assert_eq!(slice, &[1, 2, 3]);
    }

    #[test]
    fn refill_clears_previous_contents() {
        let buf: RtScratchBuf<u32, 8> = RtScratchBuf::new();
        let _ = unsafe {
            buf.fill_and_read(|v| {
                v.extend([1, 2, 3]);
            })
        };
        let slice = unsafe {
            buf.fill_and_read(|v| {
                v.push(9);
            })
        };
        assert_eq!(slice, &[9], "each fill starts from empty");
    }

    #[test]
    fn empty_fill_yields_empty_slice() {
        let buf: RtScratchBuf<u32, 8> = RtScratchBuf::new();
        let slice = unsafe { buf.fill_and_read(|_| {}) };
        assert!(slice.is_empty());
    }

    #[test]
    fn push_past_capacity_is_dropped_not_spilled() {
        let buf: RtScratchBuf<u32, 4> = RtScratchBuf::new();
        let slice = unsafe {
            buf.fill_and_read(|v| {
                for i in 0..100 {
                    v.push(i);
                }
            })
        };
        assert_eq!(
            slice,
            &[0, 1, 2, 3],
            "items past N are dropped, never spilled to the heap"
        );
    }

    #[test]
    fn push_reports_refusal_at_capacity() {
        let buf: RtScratchBuf<u32, 2> = RtScratchBuf::new();
        unsafe {
            buf.fill_and_read(|v| {
                assert!(v.push(1));
                assert!(v.push(2));
                assert!(!v.push(3), "push past N must report refusal");
                assert!(v.is_full());
                assert_eq!(v.remaining(), 0);
            })
        };
    }

    #[test]
    fn extend_reports_how_many_were_written() {
        let buf: RtScratchBuf<u32, 3> = RtScratchBuf::new();
        let slice = unsafe {
            buf.fill_and_read(|v| {
                let written = v.extend(0..10);
                assert_eq!(written, 3, "extend stops at the cap and reports it");
            })
        };
        assert_eq!(slice, &[0, 1, 2]);
    }

    #[test]
    fn capacity_is_not_a_spill_across_refills() {
        // A buffer driven past its cap every block must still be inline
        // afterwards — the whole point of the cap. Verified via `remaining`
        // rather than a heap probe, which `no_std` cannot run here.
        let buf: RtScratchBuf<u32, 4> = RtScratchBuf::new();
        for _ in 0..100 {
            unsafe {
                buf.fill_and_read(|v| {
                    v.extend(0..50);
                    assert_eq!(v.len(), 4);
                    assert_eq!(v.remaining(), 0);
                })
            };
        }
    }
}
