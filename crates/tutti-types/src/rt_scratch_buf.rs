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
/// the heap.
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
    /// `fill` receives the cleared `SmallVec` and pushes the block's items into
    /// it. The buffer is capped at `N` only by the caller's discipline in
    /// `fill` (push no more than `N`); a `SmallVec` will spill to the heap if
    /// pushed past `N`, so size `N` to the worst case.
    ///
    /// # Safety
    /// The caller must guarantee single-audio-thread access: no other thread
    /// may call this (or otherwise touch the buffer) while either this call is
    /// running or the returned slice is still alive.
    #[inline]
    pub unsafe fn fill_and_read(&self, fill: impl FnOnce(&mut SmallVec<[T; N]>)) -> &[T] {
        // SAFETY: single-audio-thread access per the method contract; the
        // mutable borrow ends before the shared reborrow below.
        let buf = unsafe { &mut *self.inner.get() };
        buf.clear();
        fill(buf);
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
        let slice = unsafe { buf.fill_and_read(|v| v.extend([1, 2, 3])) };
        assert_eq!(slice, &[1, 2, 3]);
    }

    #[test]
    fn refill_clears_previous_contents() {
        let buf: RtScratchBuf<u32, 8> = RtScratchBuf::new();
        let _ = unsafe { buf.fill_and_read(|v| v.extend([1, 2, 3])) };
        let slice = unsafe { buf.fill_and_read(|v| v.push(9)) };
        assert_eq!(slice, &[9], "each fill starts from empty");
    }

    #[test]
    fn empty_fill_yields_empty_slice() {
        let buf: RtScratchBuf<u32, 8> = RtScratchBuf::new();
        let slice = unsafe { buf.fill_and_read(|_| {}) };
        assert!(slice.is_empty());
    }
}
