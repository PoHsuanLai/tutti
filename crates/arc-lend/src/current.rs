//! Pointer to the cell's current value, and every `unsafe` that turns it
//! back into an `Arc`.

use std::marker::PhantomData;
use std::ptr::NonNull;
use std::sync::Arc;

use crate::cell::Control;
use crate::slots::Padded;
use crate::sync::{AtomicPtr, Ordering};

/// Atomic pointer to the current value, made by [`Arc::into_raw`].
///
/// The cell owns the value's one strong count through it. Every `unsafe`
/// that turns the pointer back into an `Arc` is here;
/// [`Loan`](crate::Loan)'s dereference is the one outside.
pub(crate) struct Current<T> {
    /// On a line of its own: every lend reads it, and only a publish writes
    /// it.
    ptr: Padded<AtomicPtr<T>>,
    /// Owns an `Arc<T>` through a raw pointer; this gives it (and so the
    /// cell) `Arc<T>`'s auto traits (`Send + Sync` exactly when
    /// `T: Send + Sync`) and drop-check behaviour, so no `unsafe impl` is
    /// needed.
    _owns: PhantomData<Arc<T>>,
}

impl<T> Current<T> {
    pub(crate) fn new(value: Arc<T>) -> Self {
        Self {
            ptr: Padded(AtomicPtr::new(Arc::into_raw(value).cast_mut())),
            _owns: PhantomData,
        }
    }

    /// Loads the current pointer, for a lend.
    ///
    /// `Acquire` pairs with [`swap`](Self::swap)'s `Release`, so the pointee
    /// is fully constructed from the loading thread's point of view.
    #[inline]
    pub(crate) fn load(&self) -> NonNull<T> {
        let ptr = self.ptr.0.load(Ordering::Acquire);
        // Only ever set from `Arc::into_raw`, never null.
        NonNull::new(ptr).expect("ArcLend::current is never null")
    }

    /// Swaps `value` in, and returns the outgoing value for the caller to
    /// retire.
    ///
    /// Readers may still hold the outgoing value: dropping what this returns
    /// is the caller's decision, made by the scan.
    ///
    /// `Release` publishes the new value's contents to readers' `Acquire`
    /// loads. No `Acquire` half: the outgoing value was stored by a publisher
    /// holding the control lock, which the caller holds too, so the mutex
    /// already orders its contents before us, and what readers did with it
    /// reaches the free through the scan's `Acquire` slot reads. (`AcqRel`
    /// here was redundant with the lock; see the loom model's mutation
    /// record.)
    pub(crate) fn swap(&self, value: Arc<T>) -> Arc<T> {
        let old = self
            .ptr
            .0
            .swap(Arc::into_raw(value).cast_mut(), Ordering::Release);
        // SAFETY: `old` came out of `ptr`, which only ever holds a pointer
        // from `Arc::into_raw` whose strong count the cell owns. The swap
        // removed it, so this is the one place that count is reclaimed.
        unsafe { Arc::from_raw(old) }
    }

    /// Returns a new strong count on the current value.
    ///
    /// The caller holds the cell's lock (`_locked` is the proof), so no
    /// publish can retire and free the value between the load and the
    /// increment.
    pub(crate) fn to_arc(&self, _locked: &Control<T>) -> Arc<T> {
        let ptr = self.load().as_ptr();
        // SAFETY: `ptr` is from `Arc::into_raw` with a count the cell owns,
        // and only a publish, which needs the lock the caller holds, can
        // replace (and so retire) it. The increment gives `from_raw` a count
        // of its own to take.
        unsafe {
            Arc::increment_strong_count(ptr);
            Arc::from_raw(ptr)
        }
    }
}

impl<T> Drop for Current<T> {
    fn drop(&mut self) {
        // Dropped only with the cell. `&mut` means no `Reader` is alive (each
        // holds an `Arc` of the cell), so no loan is, and no atomic access is
        // needed.
        #[cfg(not(loom))]
        let ptr = *self.ptr.0.get_mut();
        #[cfg(loom)]
        let ptr = self.ptr.0.with_mut(|p| *p);
        // SAFETY: as in `swap`: `ptr` holds a pointer from `Arc::into_raw`
        // whose count the cell owns, and nothing will read it again.
        drop(unsafe { Arc::from_raw(ptr) });
    }
}
