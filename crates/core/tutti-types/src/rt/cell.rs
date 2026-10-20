//! Lock-free interior mutability for the audio callback.
//!
//! [`AudioThreadCell`] backs a `&self`-reachable value with an `UnsafeCell`
//! and one contract — at most one borrow live at a time — rather than a
//! `Mutex`. [`BorrowGuard`] and [`BorrowRef`] are its two guards.
//!
//! Its own module because the contract is unusual enough to need stating in one
//! place: the cell checks it on every borrow, in every build, with one atomic
//! flag, so a violation is a panic (or `None`), never two live `&mut T`.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};

use sync::{AtomicBool, Ordering};

/// The flag's atomic: `std`'s in a real build, `loom`'s under `--cfg loom`, so
/// the model in `tests/cell_loom.rs` checks this code rather than a replica.
mod sync {
    #[cfg(loom)]
    pub(super) use loom::sync::atomic::{AtomicBool, Ordering};
    #[cfg(not(loom))]
    pub(super) use std::sync::atomic::{AtomicBool, Ordering};
}

/// An interior-mutability cell for the audio callback, with the contract "at
/// most one borrow active at any moment".
///
/// The cell enforces that contract itself, in release builds as in debug
/// ones: an atomic "in-use" flag is set by each borrow (a compare-exchange) and cleared
/// when its guard drops. A second borrow while one is live — from another
/// thread, or nested on the same one — panics ([`borrow_mut`], [`borrow`]) or
/// returns `None` ([`try_borrow_mut`]); it never hands out a second reference.
/// That is what makes the cell sound to share across threads (`Sync`) through
/// safe code. The cost is one uncontended atomic compare-exchange per borrow
/// and a plain store per release, `atomic_refcell`'s: no lock, no allocation, nothing that
/// waits, so it is fine on the audio thread.
///
/// A panic is the diagnostic, not the plan: a caller still keeps the cell to
/// one borrower at a time, typically by reaching it only from the audio
/// callback, and a panic means it did not.
///
/// The check is "one borrow at a time", never "always the same thread".
/// Pinning an owner thread is the stricter and *wrong* check: an OS audio stack
/// (notably CoreAudio) legitimately migrates its callback to a different thread
/// between invocations, and an owner check fires on every such migration.
///
/// [`borrow`] is exclusive too: it is a convenience for `&T` access, not a
/// reader count, so two `borrow`s cannot overlap either.
///
/// [`borrow_mut`]: Self::borrow_mut
/// [`borrow`]: Self::borrow
/// [`try_borrow_mut`]: Self::try_borrow_mut
///
/// # Why not a `Mutex<T>`
///
/// VST3's COM-object contract guarantees that the host and plugin only touch a
/// given parameter / event object during `IAudioProcessor::process`, which
/// runs single-threaded on the audio thread. Likewise the engine's own RT
/// processors are entered one call at a time. That single-borrow guarantee lets
/// these objects drop the `Mutex<T>` wrappers traditional implementations use:
/// a borrow never waits, so it cannot block the audio thread behind a UI
/// thread holding the lock, and a stray cross-thread access (a UI thread
/// reaching in) is an immediate panic instead of a silent race.
pub struct AudioThreadCell<T> {
    inner: UnsafeCell<T>,
    in_use: AtomicBool,
}

impl<T> AudioThreadCell<T> {
    /// Wraps a value. `const`, so a cell can be a `static` or a const field.
    #[cfg(not(loom))]
    pub const fn new(val: T) -> Self {
        Self {
            inner: UnsafeCell::new(val),
            in_use: AtomicBool::new(false),
        }
    }

    /// Wraps a value (`loom`'s atomics are not `const`).
    #[cfg(loom)]
    pub fn new(val: T) -> Self {
        Self {
            inner: UnsafeCell::new(val),
            in_use: AtomicBool::new(false),
        }
    }

    /// Borrows the cell mutably for the lifetime of the returned guard.
    ///
    /// # Panics
    ///
    /// If another borrow is already live, in any build. The message names
    /// this call site.
    #[inline]
    #[track_caller]
    pub fn borrow_mut(&self) -> BorrowGuard<'_, T> {
        match self.try_borrow_mut() {
            Some(guard) => guard,
            None => already_borrowed(core::panic::Location::caller()),
        }
    }

    /// Borrows the cell mutably, or returns `None` if another borrow is
    /// already live: for a caller that would rather skip than panic.
    #[inline]
    pub fn try_borrow_mut(&self) -> Option<BorrowGuard<'_, T>> {
        // Not `then_some`: that builds the guard eagerly, and dropping the
        // unused one on the `None` path would clear the live borrow's flag.
        if self.try_acquire() {
            Some(BorrowGuard {
                cell: self,
                _lent: Lent::default(),
            })
        } else {
            None
        }
    }

    /// Borrows the cell shared for the lifetime of the returned guard.
    ///
    /// The contract still allows only one borrow at a time, so this is a
    /// convenience for `&T` access; it does not allow concurrent readers.
    ///
    /// # Panics
    ///
    /// If another borrow is already live, in any build. The message names
    /// this call site.
    #[inline]
    #[track_caller]
    pub fn borrow(&self) -> BorrowRef<'_, T> {
        if !self.try_acquire() {
            already_borrowed(core::panic::Location::caller());
        }
        BorrowRef {
            cell: self,
            _lent: Lent::default(),
        }
    }

    /// Returns a mutable reference when the caller already has `&mut self`.
    ///
    /// No borrow check is needed: `&mut self` guarantees exclusivity.
    #[inline]
    pub fn get_mut(&mut self) -> &mut T {
        self.inner.get_mut()
    }

    /// Sets the in-use flag; `false` if it was already set (a borrow is live).
    ///
    /// `Acquire` pairs with [`release`](Self::release)'s `Release`: a borrow
    /// that finds the flag clear happens after the previous guard's drop, so
    /// everything that borrow wrote is visible here.
    ///
    /// A compare-exchange rather than a swap, as `atomic_refcell`'s
    /// `borrow_mut`: a refused borrow writes nothing, so the flag's only
    /// writers are the borrow that holds it and that borrow's release. The
    /// cost is the same one locked instruction. (A refused swap's write of
    /// `true` is harmless under the C++ model, but loom 0.7 does not order
    /// it before a later release by the holder, and reports a phantom
    /// refusal after every guard has dropped; see `tests/cell_loom.rs`.)
    #[inline]
    fn try_acquire(&self) -> bool {
        self.in_use
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    #[inline]
    fn release(&self) {
        self.in_use.store(false, Ordering::Release);
    }
}

/// The panic for a borrow that found another one live. Out of line and cold,
/// so the borrow's fast path is the compare-exchange and a branch.
#[cold]
#[inline(never)]
#[track_caller]
fn already_borrowed(caller: &core::panic::Location<'_>) -> ! {
    panic!(
        "AudioThreadCell concurrent borrow detected — another borrow is \
         already live. The contract is one borrow at a time.\n\
         Call site: {caller}"
    );
}

/// Gives a guard the auto traits of the `&mut T` it lends, rather than those
/// of the `&AudioThreadCell<T>` it holds.
///
/// The cell is `Sync` for `T: Send` alone, because its flag lets one guard
/// exist at a time. A guard has no such flag of its own: a `&BorrowRef`
/// shared between two threads hands each a `&T` at once, which is only sound
/// for `T: Sync`. So a guard is `Sync` only when `T` is, and `Send` when `T`
/// is (moving the one guard moves the one borrow), as `&mut T` is.
///
/// ```compile_fail
/// fn sync<S: Sync>() {}
/// // `Cell` is `Send` but not `Sync`: two threads must not share a `&Cell`.
/// sync::<tutti_types::BorrowRef<'static, core::cell::Cell<u32>>>();
/// ```
///
/// ```compile_fail
/// fn sync<S: Sync>() {}
/// sync::<tutti_types::BorrowGuard<'static, core::cell::Cell<u32>>>();
/// ```
///
/// ```
/// fn send<S: Send>() {}
/// fn sync<S: Sync>() {}
/// sync::<tutti_types::AudioThreadCell<core::cell::Cell<u32>>>();
/// send::<tutti_types::BorrowGuard<'static, core::cell::Cell<u32>>>();
/// sync::<tutti_types::BorrowGuard<'static, u32>>();
/// ```
///
/// Mutation (run): drop the field from both guards → the two `compile_fail`
/// examples compile, and the doctests fail.
type Lent<'a, T> = core::marker::PhantomData<&'a mut T>;

/// Mutable borrow guard returned by [`AudioThreadCell::borrow_mut`].
pub struct BorrowGuard<'a, T> {
    cell: &'a AudioThreadCell<T>,
    /// What the guard lends out, for the auto traits: see `Lent`.
    _lent: Lent<'a, T>,
}

impl<T> Deref for BorrowGuard<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: this guard exists only because `try_acquire` set the
        // in-use flag, and no other guard can exist until its drop clears it,
        // so nothing else references the value.
        unsafe { &*self.cell.inner.get() }
    }
}

impl<T> DerefMut for BorrowGuard<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as above; `&mut self` on the guard rules out a second
        // reference through this same guard.
        unsafe { &mut *self.cell.inner.get() }
    }
}

impl<T> Drop for BorrowGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        self.cell.release();
    }
}

/// Shared borrow guard returned by [`AudioThreadCell::borrow`].
pub struct BorrowRef<'a, T> {
    cell: &'a AudioThreadCell<T>,
    /// What the guard lends out, for the auto traits: see `Lent`.
    _lent: Lent<'a, T>,
}

impl<T> Deref for BorrowRef<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: see `BorrowGuard::deref`; this guard holds the same flag.
        unsafe { &*self.cell.inner.get() }
    }
}

impl<T> Drop for BorrowRef<'_, T> {
    #[inline]
    fn drop(&mut self) {
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

// `Send` is the auto trait's: the cell owns its `T` and an atomic, so it is
// `Send` exactly when `T` is.

// SAFETY: sharing the cell hands each thread only `borrow`/`borrow_mut`, and
// the in-use flag, checked in every build, lets one guard exist at a time. So
// at most one thread reaches the value at once, which is a `Mutex`'s
// condition: `T: Send`, not `T: Sync`, because `borrow` is exclusive too.
unsafe impl<T: Send> Sync for AudioThreadCell<T> {}

// Under `--cfg loom` the flag is loom's, which only runs inside a model:
// these run on `std` threads, and `tests/cell_loom.rs` is the model.
#[cfg(all(test, not(loom)))]
mod tests {
    extern crate std;
    use super::*;

    /// Sequential borrows on one thread: each guard's drop frees the cell.
    ///
    /// Mutation (run): `release` a no-op (the flag never clears) → the
    /// `borrow` after the first guard drops panics → fails.
    #[test]
    fn borrow_mut_from_same_thread() {
        let cell = AudioThreadCell::new(42u32);
        *cell.borrow_mut() = 100;
        assert_eq!(*cell.borrow(), 100);
        *cell.borrow_mut() += 1;
        assert_eq!(*cell.borrow(), 101);
    }

    /// After one thread's borrow drops, another thread may borrow. This is
    /// the CoreAudio callback-migration case an owner-pinning check rejects.
    ///
    /// Mutation (run): `release` a no-op → the spawned thread's borrow
    /// panics → fails.
    #[test]
    fn sequential_borrows_across_threads_are_ok() {
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

    /// A second borrow while one is live panics, in every build: this test is
    /// not `cfg(debug_assertions)`, and `--release` runs it with the check
    /// that makes the cell `Sync`.
    ///
    /// # Two barriers, and why neither is a sleep
    ///
    /// The property needs the main thread's borrow attempt to fall strictly
    /// *inside* the other thread's live guard. A sleep would make the test a
    /// bet: if the main thread is descheduled past it the guard is already
    /// gone, `borrow_mut` succeeds, and the test proves nothing about the check
    /// it names.
    ///
    /// Two barriers close the window without any duration at all. `taken` is
    /// released after the guard exists, so the main thread cannot attempt its
    /// borrow too early; `attempted` is released after the attempt has returned,
    /// so the holder cannot drop the guard too early. Between them the ordering
    /// is enforced rather than hoped for, and the test has no timing to tune.
    ///
    /// Mutation (run): gate `try_acquire`'s compare-exchange behind
    /// `cfg(debug_assertions)` (always succeeding without it), the shape the
    /// cell had → `cargo nextest run --release -p tutti-types -E
    /// 'test(concurrent_borrows_panic)'` fails (no panic: two `&mut u32`
    /// alias), while the debug run still passes.
    #[test]
    fn concurrent_borrows_panic() {
        use std::sync::{Arc, Barrier};
        let cell = Arc::new(AudioThreadCell::new(0u32));
        // "The guard is live" and "the second borrow has been attempted". Two,
        // not one: a single barrier orders only the first of those two edges.
        let taken = Arc::new(Barrier::new(2));
        let attempted = Arc::new(Barrier::new(2));

        let cell2 = Arc::clone(&cell);
        let taken2 = Arc::clone(&taken);
        let attempted2 = Arc::clone(&attempted);
        let other = std::thread::spawn(move || {
            let _g = cell2.borrow_mut();
            taken2.wait();
            // Hold the guard until the main thread's attempt has completed.
            attempted2.wait();
        });

        taken.wait();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = cell.borrow_mut();
        }));
        // The non-panicking form refuses too, and both `borrow` forms do.
        let tried = cell.try_borrow_mut().is_none();
        let shared = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = cell.borrow();
        }));
        attempted.wait();

        other.join().unwrap();
        assert!(
            result.is_err(),
            "expected panic from concurrent borrow while another borrow was live"
        );
        assert!(tried, "try_borrow_mut must refuse while a borrow is live");
        assert!(shared.is_err(), "borrow must panic while a borrow is live");

        // The guard is gone now, so the cell must be borrowable again. Without
        // this the test would also pass against a cell that had panicked itself
        // into a permanently-locked state.
        assert_eq!(*cell.borrow(), 0);
    }

    /// The panic names the call site that broke the contract, through
    /// `#[track_caller]` on `borrow_mut`.
    ///
    /// Mutation (run): drop `#[track_caller]` from `borrow_mut` → the message
    /// names `borrow_mut`'s own line in this file → fails.
    #[test]
    fn the_panic_names_the_second_borrow() {
        let cell = AudioThreadCell::new(0u32);
        let _held = cell.borrow_mut();
        let line = line!() + 2;
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = cell.borrow_mut();
        }))
        .expect_err("a nested borrow panics");
        let msg = panic
            .downcast_ref::<std::string::String>()
            .expect("a formatted panic message");
        assert!(msg.contains("concurrent borrow detected"), "{msg}");
        let site = std::format!("{}:{line}:", file!());
        assert!(msg.contains(&site), "expected {site} in {msg}");
    }

    /// A refused `try_borrow_mut` leaves the live borrow's flag alone, and a
    /// borrow after the guard drops succeeds.
    ///
    /// Mutation (run): build the guard with `bool::then_some` (eagerly, so
    /// the refused attempt's unused guard drops and clears the flag) → the
    /// second attempt gets `Some` while `held` is live → fails.
    #[test]
    fn a_refused_try_borrow_keeps_the_live_borrow() {
        let cell = AudioThreadCell::new(1u32);
        let held = cell.borrow_mut();
        assert!(cell.try_borrow_mut().is_none());
        assert!(
            cell.try_borrow_mut().is_none(),
            "a refused attempt released the live borrow"
        );
        drop(held);
        *cell.try_borrow_mut().expect("free after the drop") += 1;
        assert_eq!(*cell.borrow(), 2);
    }
}
