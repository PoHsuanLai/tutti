//! [`ArcLend`], and everything that runs on the writer's side: publishing,
//! reclaiming, and scanning.

use std::sync::Arc;

use crate::current::Current;
use crate::reader::Reader;
use crate::slots::Slots;
use crate::sync::{Mutex, MutexGuard, Ordering, fence};

/// State that only the writer's side touches, kept behind the cell's mutex.
pub(crate) struct Control<T> {
    /// Swapped-out values that may still be on loan, oldest first, each with
    /// the sequence number it had while it was current.
    retired: Vec<(Arc<T>, u64)>,
    /// Sequence number of the current value, bumped by every publish.
    seq: u64,
    /// `seq` as of the last scan of the slots.
    scanned_at: u64,
    pub(crate) slots: Slots,
    /// Reused buffer for values about to be freed, so a publish rarely
    /// allocates one. Values are moved in under the lock and dropped after
    /// it.
    pub(crate) spare: Vec<Arc<T>>,
}

impl<T> Control<T> {
    /// Moves into `freeable` every retired value no reader can still be
    /// reading.
    ///
    /// Shared by `publish` and `reclaim`: a fence, one pass over the slots,
    /// and one over the retired list (`docs::design`, §4.5). `freeable` must
    /// have room for all of `retired`. Nothing here allocates or drops, so
    /// nothing can unwind with a value half-moved.
    fn scan(&mut self, freeable: &mut Vec<Arc<T>>) {
        // The publisher half of the fence pair. Orders the swap (or, for a
        // reclaim, the last publish's swap) against every reader's claim; see
        // §5 of `docs::design`.
        fence(Ordering::SeqCst);

        let floor = self.slots.read(self.scanned_at);
        self.scanned_at = self.seq;

        // One order-preserving pass over the retired list, so values are
        // freed oldest first. `extract_if` moves each one at most once;
        // `freeable` has room for the whole list.
        let slots = &self.slots;
        let unpinned = self.retired.extract_if(.., |(value, seq)| {
            *seq < floor && !slots.is_announced(Arc::as_ptr(value).addr())
        });
        freeable.extend(unpinned.map(|(value, _)| value));
    }
}

/// A shared value that readers borrow and only the writer frees.
///
/// One thread replaces the value with [`publish`](Self::publish). Each
/// thread that reads it registers a [`Reader`] once, with
/// [`reader`](Self::reader), and borrows the value through it with
/// [`Reader::lend`]. A read is wait-free, allocation-free and free-free: it
/// returns a [`Loan`](crate::Loan), a borrow whose drop cannot run a
/// destructor. Old values are freed by `publish` or
/// [`reclaim`](Self::reclaim), on the writer's thread.
///
/// Use it for state too large for an atomic: a routing table, a set of filter
/// coefficients, a level's collision data.
///
/// # Dropping the cell
///
/// Each [`Reader`] holds an `Arc` of the cell, so whichever handle drops last
/// frees the cell, with its current value and any old ones still waiting. If
/// that is a reader, the free happens there. Keep a handle on the writer's
/// side for as long as readers exist.
///
/// What each operation costs is in [`docs::design`](crate::docs::design),
/// §7.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use arc_lend::ArcLend;
///
/// struct Settings { gains: Vec<f32> }
///
/// // 1. The cell, shared through an `Arc`.
/// let cell = Arc::new(ArcLend::new(Settings { gains: vec![1.0; 4] }));
///
/// // 2. Register the reader on the writer's side, then move it to its thread.
/// let mut reader = cell.reader();
/// let reader_thread = std::thread::spawn(move || {
///     for _iteration in 0..1000 {
///         // 3. One lend per iteration: wait-free, allocation-free, free-free.
///         let settings = reader.lend();
///         let total: f32 = settings.gains.iter().sum();
///     }
/// });
///
/// // 4. The writer publishes new versions, and frees old ones here.
/// for n in 1..100 {
///     cell.publish(Settings { gains: vec![0.5; 4 + n % 3] });
/// }
/// reader_thread.join().unwrap();
/// ```
pub struct ArcLend<T> {
    pub(crate) current: Current<T>,
    /// Everything a reader never touches: old values, sequence numbers and
    /// the slot list. The mutex also serializes writers. (Declared after
    /// `current`, so the current value is freed before the retired ones.)
    pub(crate) control: Mutex<Control<T>>,
}

impl<T> ArcLend<T> {
    /// Creates a cell holding `value`.
    ///
    /// Allocates the `Arc` that carries it.
    pub fn new(value: T) -> Self {
        Self::from_arc(Arc::new(value))
    }

    /// Creates a cell holding an existing `Arc`.
    ///
    /// The cell takes over that strong count. Other clones the caller keeps
    /// are unaffected, and the value is freed wherever its last `Arc` drops:
    /// never on a reader, since readers hold none.
    pub fn from_arc(value: Arc<T>) -> Self {
        Self {
            current: Current::new(value),
            control: Mutex::new(Control {
                // No capacity reserved: these are only ever touched on the
                // publisher side, where growing them is allowed.
                retired: Vec::new(),
                seq: 0,
                scanned_at: 0,
                slots: Slots::new(),
                spare: Vec::new(),
            }),
        }
    }

    /// Locks the writer's state, ignoring poisoning.
    ///
    /// A poisoned lock still guards consistent state: nothing that runs under
    /// it can panic between changing shared state and making it consistent
    /// again (the only allocations happen before the swap, and destructors
    /// run after the lock is released), so carrying on is correct.
    pub(crate) fn lock(&self) -> MutexGuard<'_, Control<T>> {
        self.control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Registers a new [`Reader`] of this cell.
    ///
    /// Call it on the writer's side, ahead of time: it takes the cell's lock,
    /// and may allocate. Then move the reader to the thread that reads. The
    /// reader holds an `Arc` of the cell, so it can live in a long-lived
    /// structure.
    ///
    /// Each reader keeps a slot in the cell until it drops, and the next
    /// registration reuses it. The cell never gives slots back (see
    /// [`docs::guide`](crate::docs::guide)).
    ///
    /// A reader registered and dropped at once is a mistake, and does not
    /// compile under `deny(unused_must_use)`:
    ///
    /// ```compile_fail
    /// #![deny(unused_must_use)]
    /// let cell = std::sync::Arc::new(arc_lend::ArcLend::new(1u32));
    /// cell.reader(); // registered, then dropped: nothing will lend
    /// ```
    #[must_use = "a reader dropped at once frees the slot it registered; keep it to lend"]
    pub fn reader(self: &Arc<Self>) -> Reader<T> {
        let slot = self.lock().slots.register();
        Reader::new(Arc::clone(self), slot)
    }

    /// Replaces the value, and frees old values no reader is still using.
    ///
    /// Takes a `T`, or an `Arc<T>` the caller already holds (the cell takes
    /// over that strong count, as in [`from_arc`](Self::from_arc)). Call it
    /// on the writer's thread only: it locks, may allocate, and runs
    /// destructors.
    ///
    /// It never waits on a reader. An old value a reader still holds, or may
    /// be about to read, is kept and checked again by the next `publish` or
    /// [`reclaim`](Self::reclaim). The frees run after the lock is released.
    pub fn publish(&self, value: impl Into<Arc<T>>) {
        // Wrapping a `T` allocates, here, before the lock: if it panics,
        // nothing has changed.
        let value = value.into();
        let freeable = {
            let mut control = self.lock();
            let control = &mut *control;

            // The only allocations, before anything changes. If one panics,
            // nothing has; `value` is dropped on this thread by the unwind,
            // and no reader has seen it.
            control.retired.reserve(1);
            let mut freeable = std::mem::take(&mut control.spare);
            freeable.reserve(control.retired.len() + 1);

            // Swap, under the lock (see `Current::swap`), and retire the
            // outgoing value. The push cannot allocate (reserved above).
            let old_seq = control.seq;
            control.seq += 1;
            let old = self.current.swap(value);
            control.retired.push((old, old_seq));

            control.scan(&mut freeable);
            freeable
        };
        self.free(freeable);
    }

    /// Returns the current value as an owning `Arc`.
    ///
    /// Useful for deriving the next value from the last, or for starting a
    /// copy of a structure that reads the cell, without keeping a copy of
    /// the value beside it.
    ///
    /// Call it on the writer's side: it takes the cell's lock, and the `Arc`
    /// frees the value wherever it is dropped last. A reader
    /// [`lend`](Reader::lend)s instead. Holding the `Arc` does not hold up
    /// the cell: publishing carries on as usual.
    #[must_use]
    pub fn current(&self) -> Arc<T> {
        self.current.to_arc(&self.lock())
    }

    /// Frees old values no reader is still using, without publishing.
    ///
    /// Returns how many old values are still kept afterwards, because a
    /// reader holds one or might be about to read it. Call it from a periodic
    /// task on the writer's side, to get memory back when there is nothing
    /// new to publish. Like [`publish`](Self::publish), it never waits on a
    /// reader.
    ///
    /// When nothing is waiting it is only a lock and a length check, so it
    /// can be called every tick, for every cell.
    pub fn reclaim(&self) -> usize {
        let (freeable, pinned) = {
            let mut control = self.lock();
            let control = &mut *control;
            // Nothing to free, and skipping the scan keeps exactly what
            // scanning would (`docs::design`, §4.4).
            if control.retired.is_empty() {
                return 0;
            }
            let mut freeable = std::mem::take(&mut control.spare);
            freeable.reserve(control.retired.len());
            control.scan(&mut freeable);
            (freeable, control.retired.len())
        };
        self.free(freeable);
        pinned
    }

    /// Drops `freeable` outside the lock, then keeps its buffer for the next
    /// publish.
    ///
    /// `T::drop` is arbitrary code: one that publishes to this cell, or
    /// panics, must not run under our mutex.
    fn free(&self, mut freeable: Vec<Arc<T>>) {
        freeable.clear();
        // Never waits: if another publisher holds the lock, this buffer is
        // dropped instead and the next publish allocates one.
        if let Ok(mut control) = self.control.try_lock()
            && control.spare.capacity() < freeable.capacity()
        {
            control.spare = freeable;
        }
    }

    /// Returns how many old values are waiting to be freed.
    ///
    /// It takes the cell's lock, so poll it from the writer's side. Loans
    /// keep at most one value per [`Reader`] waiting. A higher count means a
    /// reader was paused mid-read while values were published, and the next
    /// `publish` or [`reclaim`](Self::reclaim) after it resumes frees them
    /// (see [`docs::guide`](crate::docs::guide)). What to do
    /// about a count that stays high is the caller's policy: the cell itself
    /// neither asserts nor logs.
    pub fn retired_len(&self) -> usize {
        self.lock().retired.len()
    }
}

impl<T: Default> Default for ArcLend<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for ArcLend<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Take a strong count under the lock, then format with the lock
        // released: a `T::fmt` that publishes to this cell would otherwise
        // deadlock on it.
        let value = self.current();
        f.debug_tuple("ArcLend").field(&*value).finish()
    }
}
