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
/// let cell = tutti_rt::AudioThreadCell::new(core::cell::Cell::new(0u32));
/// let _r = cell.borrow();
/// ```
///
/// ```compile_fail
/// let cell = tutti_rt::AudioThreadCell::new(core::cell::Cell::new(0u32));
/// let _r = cell.try_borrow();
/// ```
///
/// ```compile_fail
/// fn sync<S: Sync>() {}
/// // A shared `&guard` hands out `&Cell` on two threads.
/// sync::<tutti_rt::BorrowGuard<'static, core::cell::Cell<u32>>>();
/// ```
///
/// ```
/// fn send<S: Send>() {}
/// fn sync<S: Sync>() {}
/// sync::<tutti_rt::AudioThreadCell<core::cell::Cell<u32>>>();
/// send::<tutti_rt::BorrowGuard<'static, core::cell::Cell<u32>>>();
/// sync::<tutti_rt::BorrowGuard<'static, u32>>();
/// send::<tutti_rt::BorrowRef<'static, u32>>();
/// sync::<tutti_rt::BorrowRef<'static, u32>>();
/// let cell = tutti_rt::AudioThreadCell::new(core::cell::Cell::new(0u32));
/// cell.borrow_mut().set(1);
/// ```
///
/// (`BorrowRef<'_, T>` is `Send`/`Sync` only for `T: Sync`, `&T`'s traits;
/// it exists only for such a `T`, so there is no `!Sync` case to pin.)
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
mod tests;
