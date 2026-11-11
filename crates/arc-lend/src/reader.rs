//! [`Reader`] and [`Loan`]: everything that runs on a reader's thread.
//! Nothing here frees, allocates or waits.

use std::cell::Cell;
use std::marker::PhantomData;
use std::ops::Deref;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::Ordering as StdOrdering;

use crate::cell::ArcLend;
use crate::slots::{FREE, Slot, Word};
use crate::sync::{Ordering, fence};

/// One thread's registration to read an [`ArcLend`].
///
/// Made by [`ArcLend::reader`] on the writer's side, then moved to the thread
/// that reads. Borrow the value with [`lend`](Self::lend), once per
/// iteration.
///
/// - **`Send`, not `Sync`.** It lends through `&mut self`, so it has at most
///   one live [`Loan`]. Code that reads one cell at two levels of nesting
///   needs one reader per level.
/// - **Cloning registers another reader**, so clone on the writer's side:
///   it locks and may allocate. The clone shares only the cell with the
///   original.
/// - **Dropping it** gives its slot back for reuse, and drops its `Arc` of
///   the cell (see [`ArcLend`], "Dropping the cell").
///
/// It is `!Sync`:
///
/// ```compile_fail
/// fn shared<X: Sync>(_: &X) {}
/// let cell = std::sync::Arc::new(arc_lend::ArcLend::new(1u32));
/// shared(&cell.reader()); // `Reader` is not `Sync`
/// ```
pub struct Reader<T> {
    /// Keeps the cell, and so this reader's slot, alive.
    cell: Arc<ArcLend<T>>,
    /// This reader's slot, in a block the cell owns. Blocks never move and are
    /// freed only with the cell, which `cell` keeps alive.
    slot: NonNull<Slot>,
    /// `Cell<()>` is `Send` and not `Sync`, and so (with the `Send` impl
    /// below) is the reader. `NonNull` alone would make it `!Sync` too; this
    /// says so on purpose rather than by accident of representation.
    _not_sync: PhantomData<Cell<()>>,
}

// SAFETY: a `Reader` is an `Arc<ArcLend<T>>` (which is `Send` exactly when
// `T: Send + Sync`) and a pointer to a slot inside that cell: atomics owned
// by this reader, two of them read by the publisher. Moving the reader
// to another thread moves that ownership with it; nothing in the slot is tied
// to the thread that registered it. It stays `!Sync` (see `_not_sync`).
unsafe impl<T: Send + Sync> Send for Reader<T> {}

impl<T> Reader<T> {
    /// Creates a reader of `cell` on `slot`, which
    /// [`Slots::register`](crate::slots::Slots) just handed out.
    pub(crate) fn new(cell: Arc<ArcLend<T>>, slot: NonNull<Slot>) -> Self {
        Self {
            cell,
            slot,
            _not_sync: PhantomData,
        }
    }

    #[inline]
    pub(crate) fn slot(&self) -> &Slot {
        // SAFETY: the slot is in a block owned by `self.cell`'s control block,
        // which never moves or frees a block before the cell drops, and
        // `self.cell` keeps the cell alive.
        unsafe { self.slot.as_ref() }
    }

    /// Returns the claim word for this reader's next lend.
    ///
    /// Generations wrap; a wrapped one that matches the claim the writer last
    /// saw only makes it keep more (`docs::design`, §5.1).
    #[inline]
    pub(crate) fn next_claim(&mut self) -> usize {
        // On the slot's line, not in `self`: readers kept side by side (a
        // `Vec<Reader>`) would otherwise share a line that every lend writes.
        let generation = &self.slot().generation;
        let next = generation.load(StdOrdering::Relaxed).wrapping_add(1);
        generation.store(next, StdOrdering::Relaxed);
        Word::claim(next)
    }

    /// Borrows the current value until the returned [`Loan`] drops.
    ///
    /// Wait-free, allocation-free and free-free, whatever the writer is
    /// doing, so it is safe on a thread with a deadline.
    ///
    /// Hold the loan for one iteration of your loop. A loan kept longer keeps
    /// its value alive until the next publish or reclaim after it drops.
    ///
    /// A reader has one live loan at a time; a second lend while the first
    /// loan is alive does not compile:
    ///
    /// ```compile_fail
    /// let cell = std::sync::Arc::new(arc_lend::ArcLend::new(1u32));
    /// let mut reader = cell.reader();
    /// let a = reader.lend();
    /// let b = reader.lend(); // a second reader is needed for this
    /// assert_eq!(*a + *b, 2);
    /// ```
    ///
    /// A loan dropped at once reads nothing, and does not compile under
    /// `deny(unused_must_use)`:
    ///
    /// ```compile_fail
    /// #![deny(unused_must_use)]
    /// let cell = std::sync::Arc::new(arc_lend::ArcLend::new(1u32));
    /// let mut reader = cell.reader();
    /// reader.lend(); // claimed, announced and released, and never read
    /// ```
    #[inline]
    #[must_use = "a loan dropped at once reads nothing; bind it for the iteration"]
    pub fn lend(&mut self) -> Loan<'_, T> {
        let claim = self.next_claim();
        let slot = self.slot();

        // Claim. `Release`: if a publisher reads this claim, everything this
        // reader did through its previous loan happens before that read, and
        // so before anything the publisher then frees (`docs::design`, §5,
        // case (b)).
        slot.word.store(claim, Ordering::Release);

        // The reader half of the fence pair. Without it, the load below may be
        // satisfied before the claim is visible, and a publisher can scan an
        // apparently free slot, free the value, and hand this reader a
        // dangling pointer.
        fence(Ordering::SeqCst);

        // `Acquire`, pairing with the publisher's `Release` swap.
        let value = self.cell.current.load();

        // Announce. `Relaxed`: the publisher only compares the address, and
        // the fence above already orders this reader's earlier accesses before
        // any scan that reads this store.
        slot.word
            .store(Word::announcement(value), Ordering::Relaxed);

        Loan {
            slot,
            value,
            _not_send: PhantomData,
        }
    }

    /// Returns the cell this reader reads.
    ///
    /// Handy for publishing to the cell, or registering another reader, from
    /// wherever the reader is kept.
    pub fn cell(&self) -> &Arc<ArcLend<T>> {
        &self.cell
    }
}

impl<T> Clone for Reader<T> {
    /// Registers another reader of the same cell, like [`ArcLend::reader`].
    fn clone(&self) -> Self {
        self.cell.reader()
    }
}

impl<T> Drop for Reader<T> {
    fn drop(&mut self) {
        // `Release`: this reader's last store to the slot happens before the
        // registration that hands the slot on. No loan is alive (each
        // borrows the reader mutably), so the slot holds `FREE`, or the
        // announcement of a loan that was `mem::forget`ed, which the next
        // registration clears.
        self.slot().owned.store(false, Ordering::Release);
    }
}

impl<T> std::fmt::Debug for Reader<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reader").finish_non_exhaustive()
    }
}

/// A borrow of an [`ArcLend`]'s value, returned by [`Reader::lend`].
///
/// Dereferences to the value. Dropping it marks the reader as done and does
/// nothing else, so it can never free the value. There is deliberately no way
/// to turn it into an owning `Arc`.
///
/// It is not `Send`, and it borrows its reader mutably: it stays on the
/// thread that took it, cannot outlive its reader, and its reader cannot lend
/// again until it drops. Hold it for one iteration of the loop.
///
/// ```compile_fail
/// let cell = std::sync::Arc::new(arc_lend::ArcLend::new(1u32));
/// let mut reader = cell.reader();
/// let loan = reader.lend();
/// std::thread::scope(|s| {
///     s.spawn(move || *loan); // `Loan` is not `Send`
/// });
/// ```
#[must_use = "the loan pins its value only while held"]
pub struct Loan<'a, T> {
    /// Slot of the reader that lent, which announces `value`.
    slot: &'a Slot,
    /// Value the reader loaded, kept alive by the announcement rather than a
    /// refcount.
    value: NonNull<T>,
    /// `*const ()` is neither `Send` nor `Sync`, which is what pins the loan
    /// to the thread that took it. (`NonNull` alone would do the same; this
    /// says so on purpose rather than by accident of representation.)
    _not_send: PhantomData<*const ()>,
}

impl<T> Deref for Loan<'_, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: `value` was loaded from `current` after this reader claimed
        // its slot, and the slot holds the claim or the announcement of
        // `value` until `drop`. A publisher frees a retired value only after
        // its fence finds no slot announcing it and no claim that could load
        // it, and the fence pair means it cannot miss this slot while having
        // retired the value this reader loaded (`docs::design`, §5). The
        // `Acquire` load makes the pointee's construction visible. The
        // borrow is tied to `&self`, which cannot outlive the loan.
        unsafe { self.value.as_ref() }
    }
}

impl<T> Drop for Loan<'_, T> {
    #[inline]
    fn drop(&mut self) {
        // `Release`: every read this thread made through the loan
        // happens-before a publisher's `Acquire` of the cleared slot, and
        // therefore before the free. Nothing here can run a destructor.
        self.slot.word.store(FREE, Ordering::Release);
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for Loan<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        (**self).fmt(f)
    }
}
