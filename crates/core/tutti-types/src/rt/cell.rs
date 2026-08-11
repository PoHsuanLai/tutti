//! Lock-free interior mutability for the audio callback.
//!
//! [`AudioThreadCell`] backs a `&self`-reachable value with a bare `UnsafeCell`
//! and one contract — at most one borrow live at a time — rather than a
//! `Mutex`. [`BorrowGuard`] and [`BorrowRef`] are its two guards.
//!
//! Its own module because the contract is unusual enough to need stating in one
//! place: debug builds catch a violation with an atomic flag, release builds
//! carry none, and the caller is what makes it sound.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};

#[cfg(debug_assertions)]
use core::sync::atomic::{AtomicBool, Ordering};

/// Interior-mutability cell whose contract is "at most one borrow active at
/// any moment". The caller is responsible for upholding that invariant —
/// typically by reaching the cell only from the audio callback.
///
/// In debug builds the cell uses an atomic "in-use" flag to *catch concurrent
/// borrows*: if a second thread enters `borrow`/`borrow_mut` while a first
/// borrow is still alive, the second call panics.
///
/// The check is "one thread at a time", never "always the same thread". Pinning
/// an owner thread is the stricter and *wrong* check: an OS audio stack
/// (notably CoreAudio) legitimately migrates its callback to a different thread
/// between invocations, and an owner check fires on every such migration.
///
/// In release builds the cell compiles down to a bare `UnsafeCell` with zero
/// overhead; the caller's invariant is what keeps it sound.
///
/// # Why this exists (and replaces `Mutex<T>`)
///
/// VST3's COM-object contract guarantees that the host and plugin only touch a
/// given parameter / event object during `IAudioProcessor::process`, which
/// runs single-threaded on the audio thread. Likewise the engine's own RT
/// processors are entered one call at a time. That single-borrow guarantee lets
/// these objects drop the `Mutex<T>` wrappers traditional implementations use
/// and back themselves with a bare [`UnsafeCell`] — no lock, no allocation, no
/// contention. The debug in-use flag turns a stray cross-thread access (e.g. a
/// UI thread reaching in) into an immediate panic instead of a silent race.
pub struct AudioThreadCell<T> {
    inner: UnsafeCell<T>,
    #[cfg(debug_assertions)]
    in_use: AtomicBool,
}

impl<T> AudioThreadCell<T> {
    /// Wraps a value. `const`, so a cell can be a `static` or a const field.
    pub const fn new(val: T) -> Self {
        Self {
            inner: UnsafeCell::new(val),
            #[cfg(debug_assertions)]
            in_use: AtomicBool::new(false),
        }
    }

    /// No-op, kept for source compatibility. Safe to delete at the call site.
    ///
    /// The cell pins no owner thread, so a device switch needs no reset.
    #[inline]
    pub fn reset_owner(&self) {}

    /// Borrow the cell mutably for the lifetime of the returned guard.
    ///
    /// # Panics (debug only)
    /// Panics if another borrow is already live on a different thread.
    #[inline]
    #[track_caller]
    pub fn borrow_mut(&self) -> BorrowGuard<'_, T> {
        #[cfg(debug_assertions)]
        self.acquire(core::panic::Location::caller());
        BorrowGuard { cell: self }
    }

    /// Borrow the cell shared for the lifetime of the returned guard.
    ///
    /// Note: the contract still allows only one borrow at a time, so this is
    /// just a convenience for `&T` access — it does not enable multiple
    /// concurrent readers.
    ///
    /// # Panics (debug only)
    /// Panics if another borrow is already live on a different thread.
    #[inline]
    #[track_caller]
    pub fn borrow(&self) -> BorrowRef<'_, T> {
        #[cfg(debug_assertions)]
        self.acquire(core::panic::Location::caller());
        BorrowRef { cell: self }
    }

    /// Returns a mutable reference when the caller already has `&mut self`.
    /// No borrow check needed — `&mut self` guarantees exclusivity.
    #[inline]
    pub fn get_mut(&mut self) -> &mut T {
        self.inner.get_mut()
    }

    #[cfg(debug_assertions)]
    #[inline]
    fn acquire(&self, caller: &core::panic::Location<'_>) {
        if self.in_use.swap(true, Ordering::Acquire) {
            panic!(
                "AudioThreadCell concurrent borrow detected — another borrow is \
                 already live. The contract is one borrow at a time.\n\
                 Call site: {caller}"
            );
        }
    }

    #[cfg(debug_assertions)]
    #[inline]
    fn release(&self) {
        self.in_use.store(false, Ordering::Release);
    }
}

/// Mutable borrow guard returned by [`AudioThreadCell::borrow_mut`].
pub struct BorrowGuard<'a, T> {
    cell: &'a AudioThreadCell<T>,
}

impl<T> Deref for BorrowGuard<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: caller upholds the single-borrow invariant; debug builds
        // additionally enforce it via the `in_use` flag set in `acquire`.
        unsafe { &*self.cell.inner.get() }
    }
}

impl<T> DerefMut for BorrowGuard<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as above; `&mut self` on the guard plus the in-use flag
        // ensure no other borrow can observe this reference.
        unsafe { &mut *self.cell.inner.get() }
    }
}

impl<T> Drop for BorrowGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        self.cell.release();
    }
}

/// Shared borrow guard returned by [`AudioThreadCell::borrow`].
pub struct BorrowRef<'a, T> {
    cell: &'a AudioThreadCell<T>,
}

impl<T> Deref for BorrowRef<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: see `BorrowGuard::deref`.
        unsafe { &*self.cell.inner.get() }
    }
}

impl<T> Drop for BorrowRef<'_, T> {
    #[inline]
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        self.cell.release();
    }
}

impl<T> core::fmt::Debug for AudioThreadCell<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Deliberately does NOT print the value. Reading it needs the very
        // borrow this type exists to hand out one at a time, so a `Debug` impl
        // that dereferenced would either take that borrow behind the caller's
        // back or trip the in-use flag it is meant to police. The `T: Debug`
        // bound is likewise omitted: requiring it would make a cell over a
        // non-Debug payload un-printable for no gain, since the payload is not
        // printed either way.
        f.debug_struct("AudioThreadCell").finish_non_exhaustive()
    }
}

// The guards DO print their value: holding one is proof the borrow is live, so
// the deref below is the borrow the caller already took, not a second one.
impl<T: core::fmt::Debug> core::fmt::Debug for BorrowGuard<'_, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(&**self, f)
    }
}

impl<T: core::fmt::Debug> core::fmt::Debug for BorrowRef<'_, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(&**self, f)
    }
}

// SAFETY: AudioThreadCell<T> is Send if T is Send — moving it to another thread
// is safe as long as access still happens from one borrow at a time.
unsafe impl<T: Send> Send for AudioThreadCell<T> {}

// SAFETY: Sync is declared so the cell can sit behind Arc<>; the debug in-use
// check catches genuine concurrent borrows, and release builds rely on the
// caller's single-borrow invariant.
unsafe impl<T: Send> Sync for AudioThreadCell<T> {}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    #[test]
    fn borrow_mut_from_same_thread() {
        let cell = AudioThreadCell::new(42u32);
        *cell.borrow_mut() = 100;
        assert_eq!(*cell.borrow(), 100);
    }

    #[test]
    fn sequential_borrows_across_threads_are_ok() {
        // After one thread's borrow drops, another thread may borrow. This is
        // the CoreAudio callback-migration case an owner-pinning check rejects.
        use std::sync::Arc;
        let cell = Arc::new(AudioThreadCell::new(0u32));
        *cell.borrow_mut() = 7;

        let cell2 = Arc::clone(&cell);
        std::thread::spawn(move || {
            *cell2.borrow_mut() = 9;
        })
        .join()
        .unwrap();

        assert_eq!(*cell.borrow(), 9);
    }

    #[test]
    #[cfg(debug_assertions)]
    fn concurrent_borrows_panic() {
        use std::sync::{Arc, Barrier};
        let cell = Arc::new(AudioThreadCell::new(0u32));
        let barrier = Arc::new(Barrier::new(2));

        let cell2 = Arc::clone(&cell);
        let barrier2 = Arc::clone(&barrier);
        let other = std::thread::spawn(move || {
            let _g = cell2.borrow_mut();
            barrier2.wait();
            // Hold the guard while the main thread tries to borrow.
            std::thread::sleep(std::time::Duration::from_millis(50));
        });

        barrier.wait();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = cell.borrow_mut();
        }));

        other.join().unwrap();
        assert!(
            result.is_err(),
            "expected panic from concurrent borrow while another borrow was live"
        );
    }
}
