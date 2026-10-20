//! Lock-free interior mutability for the audio callback.
//!
//! [`AudioThreadCell`] backs a `&self`-reachable value with an `UnsafeCell`
//! and a reader-writer contract — any number of shared borrows, or one
//! mutable borrow, never both — rather than a `Mutex` or an `RwLock`.
//! [`BorrowGuard`] and [`BorrowRef`] are its two guards.
//!
//! Its own module because the contract is unusual enough to need stating in one
//! place: the cell checks it on every borrow, in every build, with one atomic
//! word, so a violation is a panic (or `None`), never a `&mut T` aliased.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};

use sync::{AtomicUsize, Ordering};

/// The state word's atomic: `std`'s in a real build, `loom`'s under
/// `--cfg loom`, so the model in `tests/cell_loom.rs` checks this code rather
/// than a replica.
mod sync {
    #[cfg(loom)]
    pub(super) use loom::sync::atomic::{AtomicUsize, Ordering};
    #[cfg(not(loom))]
    pub(super) use std::sync::atomic::{AtomicUsize, Ordering};
}

/// The state word's writer bit: set while a [`BorrowGuard`] is live. The bits
/// below it count live [`BorrowRef`]s.
const WRITER: usize = 1 << (usize::BITS - 1);

/// An interior-mutability cell for the audio callback: any number of shared
/// borrows at once, or one mutable borrow, never both.
///
/// The cell enforces that contract itself, in release builds as in debug
/// ones, `atomic_refcell`-style: one atomic word holds a writer bit and a
/// reader count. A mutable borrow sets the writer bit with one
/// compare-exchange from zero; a shared borrow adds one to the count with a
/// compare-exchange that never succeeds while the writer bit is set. A borrow
/// that the contract refuses — a second mutable borrow, or a shared and a
/// mutable one together, from another thread or nested on the same one —
/// panics ([`borrow_mut`], [`borrow`]) or returns `None` ([`try_borrow_mut`],
/// [`try_borrow`]), and writes nothing. It never hands out a `&mut T` that
/// aliases. That is what makes the cell sound to share across threads (`Sync`)
/// through safe code. The cost is one uncontended compare-exchange per borrow
/// and one store or decrement per release: no lock, no allocation, nothing
/// that waits on another thread's progress, so it is fine on the audio thread.
///
/// A panic is the diagnostic, not the plan: a caller still keeps mutable
/// borrows to one thread at a time, typically by reaching the cell only from
/// the audio callback. Where a refusal is a legitimate runtime outcome — the
/// audio thread meeting a control-side borrow, or a plugin's worker thread
/// reading a VST3 list the plugin is also writing — the caller uses the
/// `try_` forms and degrades (skips the block, answers the COM call with a
/// failure) instead.
///
/// The check is "no conflicting borrow live", never "always the same thread".
/// Pinning an owner thread is the stricter and *wrong* check: an OS audio stack
/// (notably CoreAudio) legitimately migrates its callback to a different thread
/// between invocations, and an owner check fires on every such migration.
///
/// [`borrow_mut`]: Self::borrow_mut
/// [`borrow`]: Self::borrow
/// [`try_borrow_mut`]: Self::try_borrow_mut
/// [`try_borrow`]: Self::try_borrow
///
/// # Why not a `Mutex<T>`
///
/// VST3's COM-object contract guarantees that the host and plugin only touch a
/// given parameter / event object during `IAudioProcessor::process`. Likewise
/// the engine's own RT processors are entered one call at a time. That lets
/// these objects drop the `Mutex<T>` wrappers traditional implementations use:
/// a borrow never waits, so it cannot block the audio thread behind a UI
/// thread holding the lock, and a stray conflicting access is refused at once
/// instead of racing.
pub struct AudioThreadCell<T> {
    inner: UnsafeCell<T>,
    /// [`WRITER`] while a mutable borrow is live, else the number of live
    /// shared borrows.
    state: AtomicUsize,
}

impl<T> AudioThreadCell<T> {
    /// Wraps a value. `const`, so a cell can be a `static` or a const field.
    #[cfg(not(loom))]
    pub const fn new(val: T) -> Self {
        Self {
            inner: UnsafeCell::new(val),
            state: AtomicUsize::new(0),
        }
    }

    /// Wraps a value (`loom`'s atomics are not `const`).
    #[cfg(loom)]
    pub fn new(val: T) -> Self {
        Self {
            inner: UnsafeCell::new(val),
            state: AtomicUsize::new(0),
        }
    }

    /// Borrows the cell mutably for the lifetime of the returned guard.
    ///
    /// # Panics
    ///
    /// If any other borrow, shared or mutable, is live, in any build. The
    /// message names this call site.
    #[inline]
    #[track_caller]
    pub fn borrow_mut(&self) -> BorrowGuard<'_, T> {
        match self.try_borrow_mut() {
            Some(guard) => guard,
            None => already_borrowed(core::panic::Location::caller()),
        }
    }

    /// Borrows the cell mutably, or returns `None` if any other borrow is
    /// live: for a caller that would rather skip than panic.
    #[inline]
    pub fn try_borrow_mut(&self) -> Option<BorrowGuard<'_, T>> {
        // Not `then_some`: that builds the guard eagerly, and dropping the
        // unused one on the `None` path would release a borrow never taken.
        if self.try_acquire_writer() {
            Some(BorrowGuard {
                cell: self,
                _lent: PhantomData,
            })
        } else {
            None
        }
    }

    /// Borrows the cell shared for the lifetime of the returned guard. Any
    /// number of shared borrows may be live at once, on any threads.
    ///
    /// Only for `T: Sync`, since two threads' borrows hand each a `&T` at
    /// once. A `T` that is only `Send` (a graph of `Box<dyn Node>`) is read
    /// through [`borrow_mut`](Self::borrow_mut), which is exclusive.
    ///
    /// # Panics
    ///
    /// If a mutable borrow is live, in any build. The message names this call
    /// site.
    #[inline]
    #[track_caller]
    pub fn borrow(&self) -> BorrowRef<'_, T>
    where
        T: Sync,
    {
        match self.try_borrow() {
            Some(guard) => guard,
            None => already_borrowed(core::panic::Location::caller()),
        }
    }

    /// Borrows the cell shared, or returns `None` if a mutable borrow is
    /// live: for a caller that would rather skip than panic.
    #[inline]
    pub fn try_borrow(&self) -> Option<BorrowRef<'_, T>>
    where
        T: Sync,
    {
        if self.try_acquire_reader() {
            Some(BorrowRef {
                cell: self,
                _lent: PhantomData,
            })
        } else {
            None
        }
    }

    /// Returns a mutable reference when the caller already has `&mut self`.
    ///
    /// No borrow check is needed: `&mut self` guarantees exclusivity.
    #[inline]
    pub fn get_mut(&mut self) -> &mut T {
        self.inner.get_mut()
    }

    /// Sets the writer bit if nothing is borrowed; `false` if anything is.
    ///
    /// `Acquire` pairs with both releases' `Release`: a mutable borrow
    /// granted after a guard's drop happens after it, so the last writer's
    /// writes are visible, and no reader is still reading what it will
    /// overwrite. A refused compare-exchange writes nothing, so the word's
    /// only writers are the borrows that hold it. (A swap would also write
    /// when refused; loom 0.7 leaves such a write unordered against the
    /// holder's later release and reports a phantom refusal after every guard
    /// has dropped, though the C++ model forbids that read. See
    /// `tests/cell_loom.rs`.)
    #[inline]
    fn try_acquire_writer(&self) -> bool {
        self.state
            .compare_exchange(0, WRITER, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    /// Adds a reader unless the writer bit is set; `false` if it is.
    ///
    /// A compare-exchange loop rather than `atomic_refcell`'s
    /// unconditional `fetch_add`, so a refused reader writes nothing (as
    /// above). It retries only when another *reader* changed the count in
    /// between, so it is lock-free: some reader always makes progress, and
    /// a writer never makes a reader wait.
    #[inline]
    fn try_acquire_reader(&self) -> bool {
        let mut seen = self.state.load(Ordering::Relaxed);
        loop {
            // The writer bit is set, or the count would reach it.
            if seen >= WRITER - 1 {
                return false;
            }
            match self.state.compare_exchange_weak(
                seen,
                seen + 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(now) => seen = now,
            }
        }
    }

    #[inline]
    fn release_writer(&self) {
        // Nothing else writes the word while the writer bit is set: every
        // other borrow's compare-exchange fails without writing.
        self.state.store(0, Ordering::Release);
    }

    #[inline]
    fn release_reader(&self) {
        self.state.fetch_sub(1, Ordering::Release);
    }
}

/// The panic for a borrow that found a conflicting one live. Out of line and
/// cold, so the borrow's fast path is the compare-exchange and a branch.
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

use core::marker::PhantomData;

/// Mutable borrow guard returned by [`AudioThreadCell::borrow_mut`].
///
/// It has `&mut T`'s auto traits rather than those of the
/// `&AudioThreadCell<T>` it holds: `Send` when `T: Send` (moving the one
/// guard moves the one borrow), `Sync` when `T: Sync` (a shared `&guard`
/// hands out `&T`).
pub struct BorrowGuard<'a, T> {
    cell: &'a AudioThreadCell<T>,
    _lent: PhantomData<&'a mut T>,
}

impl<T> Deref for BorrowGuard<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: this guard exists only because `try_acquire_writer` moved
        // the word from 0 (nothing borrowed) to the writer bit, and no other
        // borrow is granted until its drop clears it, so nothing else
        // references the value.
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
        self.cell.release_writer();
    }
}

/// Shared borrow guard returned by [`AudioThreadCell::borrow`].
///
/// It has `&T`'s auto traits: `Send` and `Sync` only when `T: Sync`, since
/// either way another thread gets a `&T` while this one may hold its own.
pub struct BorrowRef<'a, T> {
    cell: &'a AudioThreadCell<T>,
    _lent: PhantomData<&'a T>,
}

impl<T> Deref for BorrowRef<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: this guard exists only because `try_acquire_reader` counted
        // it with the writer bit clear, and no mutable borrow is granted while
        // the count is nonzero, so only `&T`s reference the value.
        unsafe { &*self.cell.inner.get() }
    }
}

impl<T> Drop for BorrowRef<'_, T> {
    #[inline]
    fn drop(&mut self) {
        self.cell.release_reader();
    }
}

impl<T> core::fmt::Debug for AudioThreadCell<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Deliberately does NOT print the value. Reading it needs a borrow,
        // so a `Debug` impl that dereferenced would either take one behind the
        // caller's back or trip the check it is meant to police. The
        // `T: Debug` bound is likewise omitted: requiring it would make a cell
        // over a non-Debug payload un-printable for no gain, since the payload
        // is not printed either way.
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

/// The bounds, pinned. The cell is `Sync` for `T: Send`, a `Mutex`'s
/// condition, because only [`AudioThreadCell::borrow_mut`] is open to every
/// `T`, and it is exclusive. The shared borrows, which hand two threads a `&T`
/// at once, exist only for `T: Sync` (an `RwLock`'s condition, met at the
/// method instead of on the type, so a graph of `Box<dyn Node>`, `Send` but
/// not `Sync`, can still sit in a shared cell).
///
/// ```compile_fail
/// // `Cell` is `Send` but not `Sync`: two readers must not share a `&Cell`.
/// let cell = tutti_types::AudioThreadCell::new(core::cell::Cell::new(0u32));
/// let _r = cell.borrow();
/// ```
///
/// ```compile_fail
/// let cell = tutti_types::AudioThreadCell::new(core::cell::Cell::new(0u32));
/// let _r = cell.try_borrow();
/// ```
///
/// ```compile_fail
/// fn sync<S: Sync>() {}
/// // A shared `&guard` hands out `&Cell` on two threads.
/// sync::<tutti_types::BorrowGuard<'static, core::cell::Cell<u32>>>();
/// ```
///
/// ```
/// fn send<S: Send>() {}
/// fn sync<S: Sync>() {}
/// sync::<tutti_types::AudioThreadCell<core::cell::Cell<u32>>>();
/// send::<tutti_types::BorrowGuard<'static, core::cell::Cell<u32>>>();
/// sync::<tutti_types::BorrowGuard<'static, u32>>();
/// send::<tutti_types::BorrowRef<'static, u32>>();
/// sync::<tutti_types::BorrowRef<'static, u32>>();
/// let cell = tutti_types::AudioThreadCell::new(core::cell::Cell::new(0u32));
/// cell.borrow_mut().set(1);
/// ```
///
/// (`BorrowRef<'_, T>` is `Send`/`Sync` only for `T: Sync`, `&T`'s traits;
/// it exists only for such a `T`, so there is no `!Sync` case to pin.)
///
/// Mutations (run): both shared borrows without their `T: Sync` bound → the
/// first two examples compile (`borrow` alone cannot lose it: it calls
/// `try_borrow`); `try_borrow` alone without it → the second; `BorrowGuard`
/// with `PhantomData<()>` → the third. Each fails the doctests.
const _AUTO_TRAITS: () = ();

// SAFETY: sharing the cell hands each thread only the borrow methods, and the
// state word grants either one `&mut T` or any number of `&T`, never both. A
// `&mut T` reached from another thread needs `T: Send`, which this bound
// requires; `&T`s on two threads at once need `T: Sync`, which `borrow` and
// `try_borrow` require themselves, so no `&T` is shared without it.
unsafe impl<T: Send> Sync for AudioThreadCell<T> {}

// Under `--cfg loom` the word is loom's, which only runs inside a model:
// these run on `std` threads, and `tests/cell_loom.rs` is the model.
#[cfg(all(test, not(loom)))]
mod tests {
    extern crate std;
    use super::*;

    /// Sequential borrows on one thread: each guard's drop frees the cell.
    ///
    /// Mutation (run): `release_writer` a no-op (the bit never clears) → the
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
    /// Mutation (run): `release_writer` a no-op → the spawned thread's borrow
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
    /// Mutation (run): gate `try_acquire_writer`'s compare-exchange behind
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

    /// Shared borrows on two threads are both granted at once, and a mutable
    /// borrow is refused while either is live.
    ///
    /// The same two-barrier shape as `concurrent_borrows_panic`: the main
    /// thread's borrow falls strictly inside the other thread's.
    ///
    /// Mutation (run): `try_acquire_reader` as a compare-exchange from 0 to
    /// 1 (shared borrows exclusive, as before this cell counted readers) →
    /// the main thread's `try_borrow` is refused → fails.
    #[test]
    fn concurrent_shared_borrows_are_both_granted() {
        use std::sync::{Arc, Barrier};
        let cell = Arc::new(AudioThreadCell::new(5u32));
        let taken = Arc::new(Barrier::new(2));
        let attempted = Arc::new(Barrier::new(2));

        let cell2 = Arc::clone(&cell);
        let (taken2, attempted2) = (Arc::clone(&taken), Arc::clone(&attempted));
        let other = std::thread::spawn(move || {
            let r = cell2.borrow();
            taken2.wait();
            attempted2.wait();
            *r
        });

        taken.wait();
        let mine = cell.try_borrow().map(|r| *r);
        let writer_refused = cell.try_borrow_mut().is_none();
        attempted.wait();

        assert_eq!(other.join().unwrap(), 5);
        assert_eq!(mine, Some(5), "a second reader must be admitted");
        assert!(
            writer_refused,
            "a writer must be refused while readers are live"
        );
        *cell.borrow_mut() = 6;
        assert_eq!(*cell.borrow(), 6);
    }

    /// A reader is refused while a writer is live, writes nothing when
    /// refused, and the last reader's drop frees the cell for a writer.
    ///
    /// Mutations (run): `release_reader` a no-op → the `borrow_mut` after
    /// the readers drop panics → fails; the reader check against
    /// `WRITER - 1` dropped (a reader admitted beside the writer) →
    /// `try_borrow` returns `Some` while `w` is live → fails.
    #[test]
    fn readers_and_a_writer_exclude_each_other() {
        let cell = AudioThreadCell::new(1u32);
        let w = cell.borrow_mut();
        assert!(cell.try_borrow().is_none());
        assert!(
            cell.try_borrow().is_none(),
            "a refused reader changed the word"
        );
        drop(w);

        let a = cell.borrow();
        let b = cell.borrow();
        assert_eq!(*a + *b, 2);
        assert!(cell.try_borrow_mut().is_none());
        drop(a);
        assert!(cell.try_borrow_mut().is_none(), "one reader is still live");
        drop(b);
        *cell.borrow_mut() = 3;
        assert_eq!(*cell.borrow(), 3);
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
    /// the refused attempt's unused guard drops and clears the writer bit) →
    /// the second attempt gets `Some` while `held` is live → fails.
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
