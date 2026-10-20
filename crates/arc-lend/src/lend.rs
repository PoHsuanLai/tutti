//! The cell, [`ArcLend`], and its borrow, [`Loan`].
//!
//! The protocol and the argument that it is sound are in
//! [`docs::internal`](crate::docs::internal); this file's comments name the
//! step each line implements.

use std::marker::PhantomData;
use std::ops::Deref;
use std::ptr::NonNull;
use std::sync::Arc;

use sync::{backoff, fence, AtomicPtr, AtomicUsize, Mutex, Ordering};

/// The atomics the protocol runs on: `std`'s in a real build, `loom`'s under
/// `--cfg loom`, so the model in `tests/loom.rs` checks *this* code rather
/// than a replica of it.
mod sync {
    #[cfg(loom)]
    pub(super) use loom::{
        sync::atomic::{fence, AtomicPtr, AtomicUsize, Ordering},
        sync::Mutex,
    };
    #[cfg(not(loom))]
    pub(super) use std::{
        sync::atomic::{fence, AtomicPtr, AtomicUsize, Ordering},
        sync::Mutex,
    };

    /// One step of a publisher-side wait on a reader that is a few
    /// instructions from done. Spins briefly, then yields: a low-priority
    /// reader descheduled mid-lend must get the CPU back, or a `SCHED_FIFO`
    /// publisher spinning on it would livelock.
    pub(super) fn backoff(spins: &mut u32) {
        *spins = spins.saturating_add(1);
        #[cfg(loom)]
        loom::thread::yield_now();
        #[cfg(not(loom))]
        if *spins < 64 {
            std::hint::spin_loop();
        } else {
            std::thread::yield_now();
        }
    }
}

/// The default number of reader slots: how many [`Loan`]s into one cell can be
/// live at once before the next lend takes the overflow path.
///
/// Per *cell*, not per thread: loans from two different cells use one slot
/// each, in their own cells. Eight covers a handful of reader threads with one
/// loan each, with room to spare, and fits one cache line.
///
/// Under `--cfg loom` it is two, so the overflow path is reachable in a model
/// small enough to exhaust.
#[cfg(not(loom))]
pub const DEFAULT_SLOTS: usize = 8;
/// The default number of reader slots (two under `--cfg loom`; see the
/// non-loom documentation).
#[cfg(loom)]
pub const DEFAULT_SLOTS: usize = 2;

/// The default number of overflow epochs a cell cycles through.
///
/// One is current; the others are sealed epochs waiting for their readers to
/// leave, or free. With four, a permanently stuck overflow reader (a forgotten
/// [`Loan`]) occupies one and the rest keep cycling.
///
/// Under `--cfg loom` it is three: the fewest at which the seal's wait on
/// `loaded` matters (with two, the other epoch is always current and pins
/// everything newer), so the model can catch its removal.
#[cfg(not(loom))]
pub const DEFAULT_EPOCHS: usize = 4;
/// The default number of overflow epochs (three under `--cfg loom`; see the
/// non-loom documentation).
#[cfg(loom)]
pub const DEFAULT_EPOCHS: usize = 3;

/// The largest `EPOCHS` a cell accepts.
///
/// The epoch word packs the current epoch's index into its low
/// `ceil(log2(EPOCHS))` bits and the count of readers registered in that epoch
/// above them. At 64 epochs the index takes 6 bits, which leaves 26 bits of
/// count on a 32-bit target: far more overflow readers than can be live in one
/// epoch at once, which is all the count has to distinguish. The publisher
/// also scans every epoch for every retired value, so more epochs only cost.
pub const MAX_EPOCHS: usize = 64;

/// The largest `SLOTS` a cell accepts.
///
/// Each publish copies every slot into an array on its own stack
/// (`[usize; SLOTS]`) and scans it for every retired value, so a cell with
/// thousands of slots would cost the publisher a large stack frame and a
/// slow filter for nothing a reader gains: past a few slots per reader
/// thread, a lend only ever uses the first free one.
pub const MAX_SLOTS: usize = 1024;

/// Slot value: no reader.
const FREE: usize = 0;
/// Slot value: a reader has claimed the slot but not yet announced what it
/// read. Neither this nor [`FREE`] can collide with an announced address: an
/// `Arc`'s data pointer always points into a real heap block, past the two
/// `usize` counts at its start (even for a zero-sized `T`), so it is never 0
/// or 1.
const CLAIMED: usize = 1;

/// The slots, on their own cache line: readers write them, and keeping them
/// off the line holding `current` means a lend does not invalidate the pointer
/// every other reader is about to load.
#[repr(align(64))]
struct Slots<const SLOTS: usize>([AtomicUsize; SLOTS]);

/// The reader-visible half of one overflow epoch.
struct EpochCounters {
    /// Registrants that have finished loading the pointer.
    loaded: AtomicUsize,
    /// Registrants whose `Loan` has dropped.
    exited: AtomicUsize,
}

/// The publisher-private half of one overflow epoch.
#[derive(Clone, Copy)]
enum EpochState {
    /// Nobody registered here since it was last reset; reusable.
    Free,
    /// The epoch readers register in now. Pins every value from `start` on.
    Current { start: u64 },
    /// Closed to new readers. `registered` readers took it; they may hold any
    /// value in `start..=end`.
    Sealed {
        start: u64,
        end: u64,
        registered: usize,
    },
}

/// Everything only the publisher touches.
struct Control<T, const EPOCHS: usize> {
    /// Values swapped out but possibly still held, with their sequence number.
    retired: Vec<(Arc<T>, u64)>,
    /// The current value's sequence number.
    seq: u64,
    epochs: [EpochState; EPOCHS],
    /// Publishes that found no free epoch to advance into.
    epoch_stalls: u64,
}

/// A value one side publishes and the other side only ever *borrows*.
///
/// For state too large to pack into an atomic: a routing table, a config
/// snapshot, a coefficient set, a level's collision mesh.
///
/// # The invariant
///
/// **A reader never holds an owning handle to published state, and never
/// frees it.**
///
/// [`lend`](Self::lend) hands back a [`Loan`], which is a *borrow*. Dropping
/// it clears a reader slot or bumps a counter; it does not decrement an `Arc`
/// the way an owning handle would, and there is no code on that path that could
/// run a destructor. That matters when the reader is a thread on which a
/// deallocation must not happen (an audio callback, a game's frame tick, a
/// control loop with a deadline): if it held the last reference to a retired
/// value, it would run `free` inside its deadline.
///
/// Retired values are freed by [`publish`](Self::publish), on the publishing
/// thread, or, when a reader still holds one at that moment, by a later
/// publish or by the cell's own drop. The cost is real, but it always lands
/// where blocking is allowed.
///
/// This is structural, not probabilistic: no code path on the reader side can
/// free, whatever the interleaving with a publisher.
///
/// There is deliberately no owning read, and no way to get an `Arc` back out.
///
/// # Parameters
///
/// - `SLOTS` ([`DEFAULT_SLOTS`]): reader slots. A lend tries each at most
///   once, so it is also the bound on a lend's CASes. From 1 to
///   [`MAX_SLOTS`].
/// - `EPOCHS` ([`DEFAULT_EPOCHS`]): overflow epochs. From 2 to
///   [`MAX_EPOCHS`].
///
/// Values outside those ranges fail to compile:
///
/// ```compile_fail
/// # // Mutation: `assert!(true || SLOTS >= 1, ..)` in `VALID`; this compiles.
/// let cell: arc_lend::ArcLend<u32, 0> = arc_lend::ArcLend::new_with_params(1); // no slots
/// ```
///
/// ```compile_fail
/// # // Mutation: `true || SLOTS <= MAX_SLOTS` in `VALID`; this compiles.
/// let cell: arc_lend::ArcLend<u32, 1025> = arc_lend::ArcLend::new_with_params(1); // > MAX_SLOTS
/// ```
///
/// ```compile_fail
/// # // Mutation: `true || EPOCHS >= 2` in `VALID`; this compiles.
/// let cell: arc_lend::ArcLend<u32, 8, 1> = arc_lend::ArcLend::new_with_params(1); // one epoch
/// ```
///
/// ```compile_fail
/// # // Mutation: `true || EPOCHS <= MAX_EPOCHS` in `VALID`; this compiles
/// # // (the packing check alone admits 65: seven bits name it).
/// let cell: arc_lend::ArcLend<u32, 8, 65> = arc_lend::ArcLend::new_with_params(1); // > MAX_EPOCHS
/// ```
///
/// # Who drops the cell
///
/// The cell itself owns the current value and the retired list, so dropping
/// the last `Arc<ArcLend<_>>` frees them on whichever thread drops it. Keep a
/// publisher-side handle alive for as long as a reader might hold one.
///
/// # Cost
///
/// A lend is one CAS on a slot, a `SeqCst` fence and an `Acquire` load, plus a
/// `Release` store when the [`Loan`] drops: heavier than a plain atomic load,
/// far lighter than a lock, and **wait-free**. **Lend once per iteration of
/// the reader's loop, not per element.** See the crate docs' cost table.
///
/// # Nested loans
///
/// Each live [`Loan`] into a cell occupies one of that cell's reader slots.
/// Holding more than `SLOTS` at once (nested lends of the *same* cell, or many
/// threads lending together) is safe: the extra readers take the overflow
/// path, which is just as wait-free and allocation-free. What it costs is
/// precision in reclamation: an overflow reader pins every value that was
/// current during its epoch (usually one or two), not just the one it holds.
/// See [`docs::limitations`](crate::docs::limitations).
///
/// # Parked loans
///
/// Reclamation assumes each [`Loan`] is dropped within the iteration that
/// took it. If several overflow loans are parked across publishes (held for
/// many iterations, or passed to `mem::forget`), a publish can find no free
/// epoch to advance into, and every value retired from then on stays pinned
/// until one of them drops: memory grows by one value per publish. Nothing is
/// unsound in that state. The cell does not assert or log; it reports it
/// through [`retired_len`](Self::retired_len) and
/// [`epoch_stalls`](Self::epoch_stalls), for the publisher to poll and act on
/// with its own policy.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use arc_lend::ArcLend;
///
/// // Publisher: build and share the cell.
/// let routing = Arc::new(ArcLend::new(vec![0usize, 1]));
///
/// // Reader, once per iteration: a borrow that cannot free.
/// {
///     let table = routing.lend();
///     assert_eq!(table[1], 1);
/// }
///
/// // Publisher: swap in a new table; the old one is freed here.
/// routing.publish(Arc::new(vec![1, 0]));
/// assert_eq!(routing.lend()[0], 1);
/// ```
pub struct ArcLend<T, const SLOTS: usize = DEFAULT_SLOTS, const EPOCHS: usize = DEFAULT_EPOCHS> {
    /// The current value, from [`Arc::into_raw`]. The cell owns that one strong
    /// count; [`Drop`] gives it back.
    current: AtomicPtr<T>,
    /// The current overflow epoch's index (low bits) and registration count.
    epoch_word: AtomicUsize,
    /// Per-epoch counters overflow readers bump.
    epochs: [EpochCounters; EPOCHS],
    /// Per-reader hazard slots: `FREE`, `CLAIMED`, or the address of the value
    /// the reader holds.
    slots: Slots<SLOTS>,
    /// Touched only by `publish` and `Drop`, never by a reader: that is the
    /// whole design. The mutex also serializes concurrent publishers.
    control: Mutex<Control<T, EPOCHS>>,
    /// The cell owns `Arc<T>`s through a raw pointer; this gives it `Arc<T>`'s
    /// auto traits (`Send + Sync` exactly when `T: Send + Sync`) and drop-check
    /// behaviour, so no `unsafe impl` is needed.
    _owns: PhantomData<Arc<T>>,
}

impl<T> ArcLend<T> {
    /// Wraps an initial value, allocating the `Arc` that carries it.
    ///
    /// Publisher side, like [`publish`](Self::publish). For non-default
    /// `SLOTS`/`EPOCHS`, use [`new_with_params`](Self::new_with_params).
    pub fn new(value: T) -> Self {
        Self::new_with_params(value)
    }

    /// Builds the cell from an existing `Arc`, when the publisher already has
    /// one.
    ///
    /// The cell takes over that strong count; other clones the caller keeps are
    /// unaffected, and the value is freed wherever its *last* `Arc` drops,
    /// which is never a reader, since readers do not hold one.
    pub fn from_arc(value: Arc<T>) -> Self {
        Self::from_arc_with_params(value)
    }
}

impl<T, const SLOTS: usize, const EPOCHS: usize> ArcLend<T, SLOTS, EPOCHS> {
    /// The parameter ranges, checked when a cell is built (a
    /// post-monomorphization error, so an invalid `ArcLend<T, 0>` fails to
    /// compile the moment anything constructs one).
    const VALID: () = {
        assert!(SLOTS >= 1, "ArcLend needs at least one reader slot");
        assert!(SLOTS <= MAX_SLOTS, "ArcLend: SLOTS exceeds MAX_SLOTS");
        assert!(
            EPOCHS >= 2,
            "ArcLend needs at least two epochs: with one, the current epoch \
             can never advance and pins every value it retires"
        );
        assert!(EPOCHS <= MAX_EPOCHS, "ArcLend: EPOCHS exceeds MAX_EPOCHS");
        // The packing below holds every epoch index.
        assert!(EPOCHS <= 1 << Self::EPOCH_BITS);
    };

    /// The epoch word packs the current epoch's index into its low bits and
    /// the number of readers registered in that epoch above them: the fewest
    /// bits that name every epoch (2 for the default 4, and for loom's 3).
    const EPOCH_BITS: u32 = usize::BITS - EPOCHS.saturating_sub(1).leading_zeros();
    const EPOCH_MASK: usize = (1 << Self::EPOCH_BITS) - 1;
    /// Registration counts wrap in the word's upper bits; `loaded` and `exited`
    /// are compared against them in the same width.
    ///
    /// Not a no-op, though it looks like one on 64-bit: there the word's count
    /// would need 2^62 overflow lends in one epoch to wrap (with the default
    /// epochs). On a 32-bit target it is 2^30, about twelve days of one
    /// overflow lend per millisecond with no publish, and once the word has
    /// wrapped, the full-width `loaded` count no longer equals `registered`
    /// unless it is masked the same way. The seal's spin would then never end.
    const COUNT_MASK: usize = usize::MAX >> Self::EPOCH_BITS;

    /// [`new`](ArcLend::new), for a cell with non-default parameters.
    ///
    /// Generic over `SLOTS` and `EPOCHS`, so name them at the type:
    ///
    /// ```
    /// use arc_lend::ArcLend;
    ///
    /// let cell: ArcLend<u32, 2, 3> = ArcLend::new_with_params(7);
    /// assert_eq!(*cell.lend(), 7);
    /// ```
    ///
    /// (`new` itself is defined for the defaults only, so that
    /// `ArcLend::new(value)` infers them, the way `HashMap::new` does its
    /// hasher.)
    pub fn new_with_params(value: T) -> Self {
        Self::from_arc_with_params(Arc::new(value))
    }

    /// [`from_arc`](ArcLend::from_arc), for a cell with non-default
    /// parameters. See [`new_with_params`](Self::new_with_params).
    pub fn from_arc_with_params(value: Arc<T>) -> Self {
        #[allow(clippy::let_unit_value)]
        let () = Self::VALID;
        let mut epochs = [EpochState::Free; EPOCHS];
        // Epoch 0 is current from the start, pinning the initial value (0) on.
        epochs[0] = EpochState::Current { start: 0 };
        Self {
            current: AtomicPtr::new(Arc::into_raw(value).cast_mut()),
            epoch_word: AtomicUsize::new(0),
            epochs: std::array::from_fn(|_| EpochCounters {
                loaded: AtomicUsize::new(0),
                exited: AtomicUsize::new(0),
            }),
            slots: Slots(std::array::from_fn(|_| AtomicUsize::new(FREE))),
            control: Mutex::new(Control {
                // No capacity reserved: the list is only ever touched on the
                // publisher side, where growing it is allowed.
                retired: Vec::new(),
                seq: 0,
                epochs,
                epoch_stalls: 0,
            }),
            _owns: PhantomData,
        }
    }

    /// Lends the current value.
    ///
    /// **Safe on a thread with a deadline**: wait-free, and it neither
    /// allocates nor frees, whatever the publisher is doing.
    ///
    /// Hold the returned [`Loan`] for one iteration of your loop and no
    /// longer: a parked one keeps a retired value alive until the next
    /// [`publish`](Self::publish) after it is dropped.
    #[inline]
    pub fn lend(&self) -> Loan<'_, T, SLOTS, EPOCHS> {
        // One CAS per slot tried, never a retry of the same slot, so this is
        // bounded by SLOTS whatever else is running. `Relaxed` is enough: the
        // fence below orders the claim against the publisher's slot scan (see
        // "Slot reuse" in `docs::internal`).
        let slot = self.slots.0.iter().position(|s| {
            s.compare_exchange(FREE, CLAIMED, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        });
        let Some(slot) = slot else {
            return self.lend_overflow();
        };

        // The reader half of the fence pair. Without it, the load below may be
        // satisfied before the claim is visible, and a publisher can scan an
        // apparently empty slot, free the value, and hand this reader a
        // dangling pointer.
        fence(Ordering::SeqCst);

        // `Acquire` pairs with the publisher's `AcqRel` swap, so the pointee is
        // fully constructed from this thread's point of view.
        let ptr = self.current.load(Ordering::Acquire);

        // Announce. `Relaxed`: the publisher only compares the address. What it
        // needs ordered (our reads of the value before its free) is carried
        // by the `Release` in `Loan::drop`.
        self.slots.0[slot].store(ptr.addr(), Ordering::Relaxed);

        Loan {
            cell: self,
            value: Self::non_null(ptr),
            held: Held::Slot(slot),
            _not_send: PhantomData,
        }
    }

    /// Every slot is taken: register in the current overflow epoch instead.
    #[cold]
    fn lend_overflow(&self) -> Loan<'_, T, SLOTS, EPOCHS> {
        // Learning the epoch and registering in it are one RMW, so there is no
        // stale-epoch window. `Acquire` synchronizes with the swap that opened
        // this epoch (every RMW continues its release sequence), so the load
        // below sees the value the epoch started at, or a later one.
        let word = self
            .epoch_word
            .fetch_add(1 << Self::EPOCH_BITS, Ordering::Acquire);
        let epoch = word & Self::EPOCH_MASK;
        let ptr = self.current.load(Ordering::Acquire);
        // `Release`: our load happens before the publisher's seal reads this,
        // so the sealed range covers whatever we just loaded. (Weakening this
        // to `Relaxed` would let the load read a value published after the
        // seal: load buffering, which loom does not explore, so the model
        // cannot catch that mutation. This ordering is held by the argument
        // in `docs::internal` alone.)
        self.epochs[epoch].loaded.fetch_add(1, Ordering::Release);
        Loan {
            cell: self,
            value: Self::non_null(ptr),
            held: Held::Epoch(epoch),
            _not_send: PhantomData,
        }
    }

    #[inline]
    fn non_null(ptr: *mut T) -> NonNull<T> {
        // `current` is only ever set from `Arc::into_raw`, which is never null.
        NonNull::new(ptr).expect("ArcLend::current is never null")
    }

    /// Publishes a new value. **Publisher side only**: never call it from a
    /// thread that must not block or free.
    ///
    /// Swaps the value in, then frees every retired value no reader can still
    /// hold, including the outgoing one if nobody is reading it. It may spin
    /// briefly on a reader caught mid-lend (a handful of instructions on the
    /// reader's side; the spin backs off to `yield_now`), and it takes a mutex
    /// that serializes publishers. Both costs (the wait and the free) belong
    /// to the caller, which is the entire point: neither reaches a reader.
    ///
    /// A retired value that a reader *is* holding is not waited for; it stays
    /// retired and is freed by the next publish after the reader lets go, or
    /// when the cell drops.
    pub fn publish(&self, value: Arc<T>) {
        let freeable = {
            // Nothing below can panic between changing shared state and making
            // the control block consistent again: the only allocations (the
            // two `reserve`s) happen before the swap and before anything is
            // moved out of `retired`. So a poisoned lock (a panic in a
            // `reserve`, or in a previous publisher's `T::drop`, which runs
            // outside the lock anyway) still guards a consistent block, and
            // carrying on is correct.
            let mut control = self
                .control
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let control = &mut *control;

            // Step 1. If this panics, nothing has changed; `value` is dropped
            // on this thread by the unwind, and no reader has seen it.
            control.retired.reserve(1);
            let mut freeable = Vec::with_capacity(control.retired.len() + 1);

            // Step 2. `Release` publishes the new value's contents to readers'
            // `Acquire` loads; `Acquire` makes the outgoing value's contents
            // ours before we drop it.
            let old_seq = control.seq;
            control.seq += 1;
            let old = self
                .current
                .swap(Arc::into_raw(value).cast_mut(), Ordering::AcqRel);
            // SAFETY: `old` came out of `current`, which only ever holds a
            // pointer from `Arc::into_raw` whose strong count the cell owns.
            // The swap removed it from `current`, so this is the one place that
            // count is reclaimed. The push cannot allocate (reserved above).
            control
                .retired
                .push((unsafe { Arc::from_raw(old) }, old_seq));

            // Step 3: the publisher half of the slot fence pair.
            fence(Ordering::SeqCst);

            // Step 4.
            self.advance_epoch(control);

            // Step 5.
            self.take_unprotected(control, &mut freeable);

            freeable
        };
        // Freed outside the lock: `T::drop` is arbitrary code, and one that
        // published to this cell (or panicked) must not do it under our mutex.
        drop(freeable);
    }

    /// Open a free epoch and seal the current one, if a free epoch exists.
    ///
    /// If none does (every other epoch still has readers in it), the current
    /// epoch stays open and keeps pinning everything from its start, until a
    /// later publish finds one free.
    fn advance_epoch(&self, control: &mut Control<T, EPOCHS>) {
        let free = (0..EPOCHS).find(|&k| match control.epochs[k] {
            EpochState::Free => true,
            EpochState::Current { .. } => false,
            EpochState::Sealed { registered, .. } => {
                // `Acquire` pairs with each reader's `Release` exit.
                let exited = self.epochs[k].exited.load(Ordering::Acquire);
                exited & Self::COUNT_MASK == registered
            }
        });
        let Some(next) = free else {
            control.epoch_stalls += 1;
            return;
        };

        // Every registrant of `next` has exited, and nobody can register in it
        // until the swap below names it, so the counters can be reset. The
        // swap's `Release` orders these stores before any new registrant's
        // increments.
        self.epochs[next].loaded.store(0, Ordering::Relaxed);
        self.epochs[next].exited.store(0, Ordering::Relaxed);
        control.epochs[next] = EpochState::Current { start: control.seq };

        let word = self.epoch_word.swap(next, Ordering::AcqRel);
        let closing = word & Self::EPOCH_MASK;
        let registered = word >> Self::EPOCH_BITS;
        let EpochState::Current { start } = control.epochs[closing] else {
            unreachable!("the epoch word always names the current epoch");
        };

        // Wait for the closing epoch's readers to finish loading, so the seal
        // bounds what they can hold. Each is past its registration and a load
        // away from done.
        let mut spins = 0;
        while self.epochs[closing].loaded.load(Ordering::Acquire) & Self::COUNT_MASK != registered {
            backoff(&mut spins);
        }
        // They loaded a value no newer than the one just published.
        control.epochs[closing] = EpochState::Sealed {
            start,
            end: control.seq,
            registered,
        };
    }

    /// Move into `freeable` every retired value no reader can still be holding.
    /// `freeable` has room for all of `retired`, so this neither allocates nor
    /// drops: nothing here can unwind with a value half-removed.
    fn take_unprotected(&self, control: &mut Control<T, EPOCHS>, freeable: &mut Vec<Arc<T>>) {
        let mut held = [FREE; SLOTS];
        for (held, slot) in held.iter_mut().zip(&self.slots.0) {
            // `Acquire`, pairing with `Loan::drop`'s `Release`: a slot seen
            // `FREE` after a lend means that reader's accesses are done.
            let mut seen = slot.load(Ordering::Acquire);
            // A reader between claim and announce. It is running a fence and
            // a load, not waiting on anything, so this ends as soon as it is
            // scheduled, and the waiting is on the publisher's thread.
            let mut spins = 0;
            while seen == CLAIMED {
                backoff(&mut spins);
                seen = slot.load(Ordering::Acquire);
            }
            *held = seen;
        }

        let pinned_by_epoch = |seq: u64| {
            (0..EPOCHS).any(|k| match control.epochs[k] {
                EpochState::Free => false,
                EpochState::Current { start } => seq >= start,
                EpochState::Sealed {
                    start,
                    end,
                    registered,
                } => {
                    (start..=end).contains(&seq)
                        && self.epochs[k].exited.load(Ordering::Acquire) & Self::COUNT_MASK
                            != registered
                }
            })
        };

        let mut i = 0;
        while i < control.retired.len() {
            let (arc, seq) = &control.retired[i];
            if held.contains(&Arc::as_ptr(arc).addr()) || pinned_by_epoch(*seq) {
                i += 1;
            } else {
                // Order-preserving, so values are freed oldest first. `remove`
                // cannot panic for an in-range index, and `push` cannot
                // allocate: `freeable` was sized for the whole list.
                freeable.push(control.retired.remove(i).0);
            }
        }
    }

    /// How many readers currently hold a [`Loan`] on the overflow path.
    #[cfg(all(test, not(loom)))]
    fn overflow_readers(&self) -> usize {
        let control = self.control.lock().unwrap();
        (0..EPOCHS)
            .map(|k| {
                let exited = self.epochs[k].exited.load(Ordering::SeqCst);
                match control.epochs[k] {
                    EpochState::Free => 0,
                    EpochState::Current { .. } => {
                        (self.epoch_word.load(Ordering::SeqCst) >> Self::EPOCH_BITS) - exited
                    }
                    EpochState::Sealed { registered, .. } => registered - exited,
                }
            })
            .sum()
    }

    /// Returns how many swapped-out values are still waiting to be freed.
    ///
    /// **Publisher side**: it takes the publisher's lock.
    ///
    /// For the publisher to poll. It should stay small: a few values plus one
    /// per slot reader parked across publishes. If it keeps growing,
    /// reclamation has stalled (see "Parked loans" above); what to do about it
    /// (log, assert in a debug build, drop a subsystem) is the caller's
    /// policy, which is why the cell itself neither asserts nor logs.
    pub fn retired_len(&self) -> usize {
        self.control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retired
            .len()
    }

    /// Returns how many publishes found every overflow epoch still occupied,
    /// and so could not start a new one.
    ///
    /// **Publisher side**: it takes the publisher's lock.
    ///
    /// A counter rather than a log line, for the publisher to poll. It stays at
    /// zero while loans are held only within an iteration. It rises when
    /// several long-lived overflow loans occupy every epoch, and while that
    /// lasts, every retired value stays pinned (see "Parked loans" above).
    pub fn epoch_stalls(&self) -> u64 {
        self.control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .epoch_stalls
    }
}

impl<T, const SLOTS: usize, const EPOCHS: usize> Drop for ArcLend<T, SLOTS, EPOCHS> {
    fn drop(&mut self) {
        // `&mut self` means no `Loan` is alive (each borrows the cell), so
        // nothing is protected, everything can go, and no atomic access is
        // needed.
        #[cfg(not(loom))]
        let current = *self.current.get_mut();
        #[cfg(loom)]
        let current = self.current.with_mut(|p| *p);
        // SAFETY: as in `publish`: `current` holds a pointer from
        // `Arc::into_raw` whose count the cell owns, and the cell is being
        // destroyed, so nothing will read `current` again.
        drop(unsafe { Arc::from_raw(current) });
        // The retired list's `Arc`s drop with the mutex, after this body.
    }
}

impl<T: Default, const SLOTS: usize, const EPOCHS: usize> Default for ArcLend<T, SLOTS, EPOCHS> {
    fn default() -> Self {
        Self::new_with_params(T::default())
    }
}

impl<T: std::fmt::Debug, const SLOTS: usize, const EPOCHS: usize> std::fmt::Debug
    for ArcLend<T, SLOTS, EPOCHS>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ArcLend").field(&*self.lend()).finish()
    }
}

/// Which registration a [`Loan`] must undo.
#[derive(Clone, Copy)]
enum Held {
    Slot(usize),
    Epoch(usize),
}

/// A borrow of the value inside an [`ArcLend`], valid for as long as it is
/// held.
///
/// Returned by [`ArcLend::lend`]. Deliberately not `Send`, and tied to the
/// cell's lifetime: a loan cannot be moved to another thread or outlive the
/// cell. Hold it for one iteration of the reader's loop and drop it.
///
/// There is no way to get an owning `Arc` back out. That is not an oversight.
/// Dropping one clears a slot or bumps a counter and does nothing else: it
/// cannot free.
///
/// ```compile_fail
/// # // Mutation: `unsafe impl<T: Sync, ..> Send for Loan<'_, T, ..> {}`; this
/// # // compiles (and `a_loan_is_not_send` fails its const assertion).
/// use arc_lend::ArcLend;
/// let cell = ArcLend::new(1u32);
/// let loan = cell.lend();
/// std::thread::scope(|s| {
///     s.spawn(move || *loan); // `Loan` is not `Send`
/// });
/// ```
pub struct Loan<'a, T, const SLOTS: usize = DEFAULT_SLOTS, const EPOCHS: usize = DEFAULT_EPOCHS> {
    cell: &'a ArcLend<T, SLOTS, EPOCHS>,
    /// The value this reader loaded. Kept alive by its registration, not by a
    /// refcount.
    value: NonNull<T>,
    held: Held,
    /// `*const ()` is neither `Send` nor `Sync`, which is what pins the loan
    /// to the thread that took it. (`NonNull` alone would do the same; this
    /// says so on purpose rather than by accident of representation.)
    _not_send: PhantomData<*const ()>,
}

impl<T, const SLOTS: usize, const EPOCHS: usize> Deref for Loan<'_, T, SLOTS, EPOCHS> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: `value` was loaded from `current` after this reader
        // registered, and the registration stays in place until `drop`. On the
        // slot path, a publisher frees a retired value only after its fence
        // finds no slot announcing it, and the fence pair means it cannot miss
        // this slot while having retired the value this reader loaded. On the
        // overflow path, the reader's epoch stays live until `drop`, and a
        // live epoch pins every value its readers can have loaded
        // (`docs::internal`, "Why it is sound"). The `Acquire` load makes the
        // pointee's construction visible. The borrow is tied to `&self`, which
        // cannot outlive the registration.
        unsafe { self.value.as_ref() }
    }
}

impl<T, const SLOTS: usize, const EPOCHS: usize> Drop for Loan<'_, T, SLOTS, EPOCHS> {
    #[inline]
    fn drop(&mut self) {
        // `Release`: every read this thread made through the loan
        // happens-before a publisher's `Acquire` of the cleared slot or the
        // exit count, and therefore before the free. Nothing here can run a
        // destructor.
        match self.held {
            Held::Slot(slot) => self.cell.slots.0[slot].store(FREE, Ordering::Release),
            Held::Epoch(epoch) => {
                self.cell.epochs[epoch]
                    .exited
                    .fetch_add(1, Ordering::Release);
            }
        }
    }
}

impl<T: std::fmt::Debug, const SLOTS: usize, const EPOCHS: usize> std::fmt::Debug
    for Loan<'_, T, SLOTS, EPOCHS>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        (**self).fmt(f)
    }
}

// Under `--cfg loom` the atomics above are loom's, which panic outside a
// `loom::model`, so these ordinary tests are compiled out; the loom model is
// `tests/loom.rs`.
#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn a_lend_sees_the_published_value() {
        let cell = ArcLend::new(vec![1u32, 2, 3]);
        assert_eq!(&*cell.lend(), &[1, 2, 3]);

        cell.publish(Arc::new(vec![4, 5]));
        assert_eq!(&*cell.lend(), &[4, 5]);
    }

    #[test]
    fn a_held_loan_keeps_seeing_its_own_snapshot() {
        // The iteration-scoped contract in miniature: a reader that took a
        // value before a publish keeps working with it, rather than tearing
        // mid-iteration.
        let cell = ArcLend::new(vec![1u32]);
        let held = cell.lend();

        cell.publish(Arc::new(vec![9]));

        assert_eq!(&*held, &[1], "guard still sees the snapshot it took");
        assert_eq!(&*cell.lend(), &[9], "a fresh read sees the new value");
    }

    #[test]
    fn publish_frees_the_retired_value_on_the_publishing_thread() {
        // The property the whole type exists for. `dropped` flips when the
        // retired payload is freed; it must be observable immediately after
        // `publish` returns, i.e. on this thread — never deferred to whichever
        // thread happens to read next.
        use std::sync::atomic::{AtomicBool, Ordering};

        struct Tattle(Arc<AtomicBool>);
        impl Drop for Tattle {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let cell = ArcLend::new(Tattle(dropped.clone()));

        assert!(!dropped.load(Ordering::SeqCst));
        cell.publish(Arc::new(Tattle(Arc::new(AtomicBool::new(false)))));
        assert!(
            dropped.load(Ordering::SeqCst),
            "retired value must be freed by publish(), on the publishing thread"
        );
    }

    /// A guard must not be able to escape the thread that took it — that is what
    /// makes "moved to another thread" a compile error rather than a convention.
    ///
    /// Asserted via autoref specialization: the inherent const on `Wrap<T>` wins
    /// over the trait's default only when `T: Send`, so `IS_SEND` resolves to
    /// `false` exactly when the type is *not* `Send`. A plain
    /// `assert_not_send::<T>()` helper cannot express this — there is no stable
    /// negative bound — and a bare `assert_send` pins the opposite claim.
    ///
    /// # Why `const` rather than `assert!`
    ///
    /// Both operands are compile-time constants, so a runtime `assert!` is the
    /// wrong tool twice over: clippy flags it as an assertion on a constant, and
    /// — the part that matters — it only runs if someone runs the tests. A
    /// `const` block is evaluated during compilation, so this property fails the
    /// *build*. That matches what it guards: `Loan`'s non-`Send`ness is a
    /// type-system claim, and putting the guarantee in the return type is what
    /// stops it depending on anyone remembering to check.
    #[test]
    fn a_loan_is_not_send() {
        struct Wrap<T>(PhantomData<T>);

        trait NotSend {
            const IS_SEND: bool = false;
        }
        impl<T> NotSend for Wrap<T> {}

        impl<T: Send> Wrap<T> {
            const IS_SEND: bool = true;
        }

        // A loan that escapes its thread outlives the iteration it was taken for.
        const {
            assert!(!<Wrap<Loan<'static, Vec<u32>>>>::IS_SEND);
        }
        // Control: the same machinery reports `true` for a type that IS `Send`,
        // so the assertion above is measuring something rather than always
        // holding. Without this, a broken `Wrap` that reported `false` for
        // everything would look like a pass.
        const {
            assert!(<Wrap<Vec<u32>>>::IS_SEND);
        }
    }

    /// The cell itself must stay shareable: every call site holds an
    /// `Arc<ArcLend<_>>` across threads. The raw pointer inside would make it
    /// neither `Send` nor `Sync` by default; `PhantomData<Arc<T>>` plus the
    /// atomics is what gives it `Arc<T>`'s auto traits back without an
    /// `unsafe impl`.
    ///
    /// Mutation: replace `_owns: PhantomData<Arc<T>>` with
    /// `PhantomData<*const T>` — this stops compiling.
    #[test]
    fn the_cell_is_send_and_sync_for_send_sync_payloads() {
        fn assert_send_sync<X: Send + Sync>() {}
        assert_send_sync::<ArcLend<Vec<u32>>>();
    }

    /// Records the thread that dropped it, so a test can say *where* a value
    /// was freed, not just that it was.
    struct DropSite {
        id: usize,
        sites: Arc<std::sync::Mutex<Vec<(usize, std::thread::ThreadId)>>>,
    }
    impl Drop for DropSite {
        fn drop(&mut self) {
            self.sites
                .lock()
                .unwrap()
                .push((self.id, std::thread::current().id()));
        }
    }

    /// A reader that lets go of a retired value does not free it — the free
    /// is deferred to the next publish, on the publisher's thread. This is the
    /// property an `ArcSwap` cannot promise: there, the last holder of a
    /// settled debt runs the destructor wherever it is.
    ///
    /// Mutation: make `Loan::drop` free the retired list (call
    /// `take_unprotected` and drop the result after clearing its slot). The
    /// reader thread then appears in `sites` and the first assertion fails.
    #[test]
    fn a_reader_releasing_a_retired_value_never_frees_it() {
        use std::sync::mpsc;

        let sites = Arc::new(std::sync::Mutex::new(Vec::new()));
        let make = |id| DropSite {
            id,
            sites: sites.clone(),
        };
        let cell = Arc::new(ArcLend::new(make(0)));

        let (took_tx, took_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let reader = {
            let cell = cell.clone();
            std::thread::spawn(move || {
                let held = cell.lend();
                assert_eq!(held.id, 0);
                took_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                // The value is retired by now; dropping the only reader of it
                // is exactly the moment an owning handle would free.
                drop(held);
                std::thread::current().id()
            })
        };

        took_rx.recv().unwrap();
        cell.publish(Arc::new(make(1)));
        assert!(
            sites.lock().unwrap().is_empty(),
            "value 0 is held by the reader and must not be freed yet"
        );
        assert_eq!(cell.retired_len(), 1);

        release_tx.send(()).unwrap();
        let reader_thread = reader.join().unwrap();
        assert!(
            sites.lock().unwrap().is_empty(),
            "releasing the read must not free anything — on any thread"
        );

        // The next publish reclaims it, here.
        cell.publish(Arc::new(make(2)));
        let here = std::thread::current().id();
        assert_eq!(*sites.lock().unwrap(), vec![(0, here), (1, here)]);
        assert_ne!(here, reader_thread);
        assert_eq!(cell.retired_len(), 0);
    }

    /// More simultaneous reads than there are slots: the extra readers take
    /// the overflow counter, see the right value, and hold it safely across a
    /// publish; nothing is freed while any of them is live, and the slots come
    /// back into use afterwards.
    ///
    /// The overflow readers hold a value *no slot announces* (the slots all
    /// hold the older one), so the only thing keeping it alive is the counter.
    ///
    /// Mutation: in `take_unprotected`'s `pinned_by_epoch`, make a `Sealed`
    /// epoch pin nothing. The second publish then frees value 1 while the
    /// overflow readers still hold it, and `sites` is non-empty at the first
    /// check.
    #[test]
    fn nested_loans_past_the_slot_count_overflow_safely() {
        let sites = Arc::new(std::sync::Mutex::new(Vec::new()));
        let make = |id| DropSite {
            id,
            sites: sites.clone(),
        };
        let here = std::thread::current().id();
        let cell = ArcLend::new(make(0));

        let in_slots: Vec<_> = (0..DEFAULT_SLOTS).map(|_| cell.lend()).collect();
        assert_eq!(cell.overflow_readers(), 0, "these fit in the slots");
        cell.publish(Arc::new(make(1)));

        let overflowed: Vec<_> = (0..2).map(|_| cell.lend()).collect();
        assert_eq!(cell.overflow_readers(), 2, "two reads past the slots");
        assert!(overflowed.iter().all(|r| r.id == 1));

        cell.publish(Arc::new(make(2)));
        assert!(
            sites.lock().unwrap().is_empty(),
            "value 1 is pinned by the overflow readers' sealed epoch"
        );
        // A read taken now sees the newest value even while older ones are
        // held (on the overflow path too — every slot is still taken).
        assert_eq!(cell.lend().id, 2);
        assert!(
            in_slots.iter().all(|r| r.id == 0),
            "held reads keep their value"
        );
        assert!(
            overflowed.iter().all(|r| r.id == 1),
            "held reads keep their value"
        );

        drop(overflowed);
        assert_eq!(cell.overflow_readers(), 0);
        cell.publish(Arc::new(make(3)));
        assert_eq!(
            *sites.lock().unwrap(),
            vec![(1, here), (2, here)],
            "with the counter clear, only the slot-held value 0 survives"
        );

        drop(in_slots);
        cell.publish(Arc::new(make(4)));
        assert_eq!(
            *sites.lock().unwrap(),
            vec![(1, here), (2, here), (0, here), (3, here)]
        );

        // Slots are reusable: a fresh read is back off the overflow path.
        let r = cell.lend();
        assert_eq!(r.id, 4);
        assert_eq!(cell.overflow_readers(), 0);
    }

    /// An overflow `Loan` passed to `mem::forget` never exits its epoch, so
    /// the values that were current during that epoch stay pinned forever —
    /// but only those. Publishing on regardless keeps the retired list at a
    /// constant size, rather than leaking one value per publish.
    ///
    /// Mutation: make `advance_epoch` return before opening a new epoch (the
    /// old single-counter design, where overflow stalled all reclamation).
    /// The current epoch then pins every retired value, and the bound fails
    /// within the first few publishes.
    #[test]
    fn a_forgotten_overflow_ref_does_not_stall_reclamation() {
        let sites = Arc::new(std::sync::Mutex::new(Vec::new()));
        let make = |id| DropSite {
            id,
            sites: sites.clone(),
        };
        let cell = ArcLend::new(make(0));

        let in_slots: Vec<_> = (0..DEFAULT_SLOTS).map(|_| cell.lend()).collect();
        let forgotten = cell.lend();
        assert_eq!(cell.overflow_readers(), 1);
        std::mem::forget(forgotten);
        drop(in_slots);

        for id in 1..=200 {
            cell.publish(Arc::new(make(id)));
            assert!(
                cell.retired_len() <= 2,
                "publish {id}: {} retired — reclamation stalled",
                cell.retired_len()
            );
        }
        // The forgotten reader's epoch pins the values current while it was
        // open: 0 (read) and 1 (published before the seal). Everything else
        // went.
        let mut freed: Vec<_> = sites.lock().unwrap().iter().map(|&(id, _)| id).collect();
        freed.sort_unstable();
        assert_eq!(freed, (2..200).collect::<Vec<_>>());
        assert_eq!(cell.overflow_readers(), 1, "still registered, forever");
    }

    /// The same bound with the overflow reader genuinely live — slots all
    /// held, one more reader past them — across many publishes, and the held
    /// reads keep their values throughout.
    ///
    /// Mutation: as above (never advance the epoch). The list grows by one
    /// per publish and the bound fails within the first few publishes.
    #[test]
    fn a_live_overflow_reader_keeps_the_retired_list_bounded() {
        let cell = ArcLend::new(0u32);
        let in_slots: Vec<_> = (0..DEFAULT_SLOTS).map(|_| cell.lend()).collect();
        let over = cell.lend();
        for v in 1..=200u32 {
            cell.publish(Arc::new(v));
            assert!(
                cell.retired_len() <= 2,
                "publish {v}: {}",
                cell.retired_len()
            );
            assert_eq!(*over, 0);
            assert!(in_slots.iter().all(|r| **r == 0));
            // A fresh read (overflow too — every slot is taken) sees the news.
            assert_eq!(*cell.lend(), v);
        }
    }

    /// The design's limit, and its release-mode observables: park an
    /// overflow `Loan` in every epoch but the current one and the current
    /// epoch can no longer advance. Each publish counts a stall, and the
    /// retired list grows. Dropping the parked refs ends it: the next publish
    /// advances again and reclaims everything.
    ///
    /// Mutation: delete `control.epoch_stalls += 1` in `advance_epoch`. The
    /// stall count stays 0 and the first stall assertion fails.
    #[test]
    fn every_epoch_parked_is_observable_and_recovers() {
        let cell = ArcLend::new(0u32);
        let in_slots: Vec<_> = (0..DEFAULT_SLOTS).map(|_| cell.lend()).collect();
        // One overflow ref per epoch but the last: each publish seals the
        // epoch the previous read landed in and opens the next.
        let mut parked = Vec::new();
        for v in 1..DEFAULT_EPOCHS as u32 {
            parked.push(cell.lend());
            cell.publish(Arc::new(v));
        }
        assert_eq!(
            cell.epoch_stalls(),
            0,
            "every advance so far found a free epoch"
        );
        parked.push(cell.lend()); // the current epoch is occupied too

        let before = cell.retired_len();
        for v in 0..10u32 {
            cell.publish(Arc::new(100 + v));
        }
        assert_eq!(cell.epoch_stalls(), 10, "every publish stalled");
        assert_eq!(
            cell.retired_len(),
            before + 10,
            "and pinned what it retired"
        );

        drop(parked);
        drop(in_slots);
        cell.publish(Arc::new(1000));
        assert_eq!(cell.epoch_stalls(), 10, "a free epoch again");
        cell.publish(Arc::new(1001));
        assert_eq!(cell.retired_len(), 0, "and everything reclaimed");
    }

    /// A retired value's destructor runs *after* the control lock is
    /// released, so a destructor that panics neither poisons the lock nor
    /// leaves the retired list half-updated; the cell keeps working.
    ///
    /// (The other panic site the lock guards against, a failed `reserve`,
    /// cannot be provoked here without an allocator that fails on demand; it
    /// happens before the swap, which is what makes it harmless.)
    ///
    /// Mutation: move `drop(freeable)` inside the locked block in `publish`.
    /// The panic then unwinds through the guard and the lock is poisoned.
    #[test]
    fn a_panicking_destructor_does_not_poison_the_cell() {
        struct Bomb(bool);
        impl Drop for Bomb {
            fn drop(&mut self) {
                if self.0 {
                    panic!("boom");
                }
            }
        }

        let cell = ArcLend::new(Bomb(true));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cell.publish(Arc::new(Bomb(false)));
        }));
        assert!(result.is_err(), "the retired value's drop panicked");
        assert!(!cell.control.is_poisoned());
        assert_eq!(cell.retired_len(), 0, "it left the list before it ran");

        cell.publish(Arc::new(Bomb(false)));
        assert!(!cell.lend().0);
        assert_eq!(cell.retired_len(), 0);
    }

    /// A reader holding a *slot* protects exactly its own value: a publish
    /// frees every other retired value, and keeps that one.
    ///
    /// Mutation: in `take_unprotected`, drop the `held.contains(..)` check
    /// (free everything retired). Value 0 is then freed while `held` still
    /// points at it — `sites` gains `(0, _)` at the first check.
    #[test]
    fn a_slot_protects_only_the_value_it_announced() {
        let sites = Arc::new(std::sync::Mutex::new(Vec::new()));
        let make = |id| DropSite {
            id,
            sites: sites.clone(),
        };
        let cell = ArcLend::new(make(0));

        let held = cell.lend();
        cell.publish(Arc::new(make(1)));
        cell.publish(Arc::new(make(2)));
        let here = std::thread::current().id();
        assert_eq!(
            *sites.lock().unwrap(),
            vec![(1, here)],
            "1 was never read, so it goes; 0 is held and stays"
        );
        assert_eq!(held.id, 0);
        drop(held);

        drop(cell);
        let mut ids: Vec<_> = sites.lock().unwrap().iter().map(|&(id, _)| id).collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec![0, 1, 2],
            "dropping the cell frees the rest, once each"
        );
    }

    /// Concurrent readers against a publisher hammering the cell.
    ///
    /// Each published value is a vector whose every element equals its
    /// generation, and whose drop poisons it and records the thread. Readers
    /// check the value is whole (not torn, not poisoned), and at the end every
    /// generation has been dropped exactly once, and never on a reader thread.
    ///
    /// Mutations: remove either `SeqCst` fence — miri's weak-memory emulation
    /// then reports a data race on the freed value, on some seeds (16 seeds,
    /// as the CI step runs, catch both; one seed alone can miss). The plain
    /// run does not on x86, whose CAS is already a full barrier; this is why
    /// the loom model exists.
    /// Remove the slot check in `take_unprotected` — readers see poisoned
    /// vectors and the whole-value assertion fails in the plain run.
    #[test]
    fn concurrent_readers_and_a_publisher_hammering() {
        use std::sync::atomic::{AtomicBool, Ordering};

        const READERS: usize = 3;
        let publishes: usize = if cfg!(miri) { 40 } else { 20_000 };
        const LEN: usize = 16;
        const POISON: usize = usize::MAX;

        struct Gen {
            v: Vec<usize>,
            sites: Arc<std::sync::Mutex<Vec<(usize, std::thread::ThreadId)>>>,
        }
        impl Drop for Gen {
            fn drop(&mut self) {
                let g = self.v[0];
                self.v.iter_mut().for_each(|x| *x = POISON);
                self.sites
                    .lock()
                    .unwrap()
                    .push((g, std::thread::current().id()));
            }
        }

        let sites = Arc::new(std::sync::Mutex::new(Vec::new()));
        let make = |g| Gen {
            v: vec![g; LEN],
            sites: sites.clone(),
        };
        let cell = Arc::new(ArcLend::new(make(0)));
        let done = Arc::new(AtomicBool::new(false));

        let readers: Vec<_> = (0..READERS)
            .map(|_| {
                let (cell, done) = (cell.clone(), done.clone());
                std::thread::spawn(move || {
                    let mut last = 0;
                    let mut reads = 0usize;
                    while !done.load(Ordering::Relaxed) || reads == 0 {
                        let r = cell.lend();
                        let g = r.v[0];
                        assert_ne!(g, POISON, "read a freed value");
                        assert!(r.v.iter().all(|&x| x == g), "torn value {:?}", r.v);
                        assert!(g >= last, "went back in time: {g} after {last}");
                        last = g;
                        reads += 1;
                        // Occasionally nest past the slot count too.
                        if reads.is_multiple_of(64) {
                            let nested: Vec<_> = (0..DEFAULT_SLOTS).map(|_| cell.lend()).collect();
                            assert!(nested.iter().all(|n| n.v[0] >= g && n.v[0] != POISON));
                        }
                    }
                    std::thread::current().id()
                })
            })
            .collect();

        for g in 1..=publishes {
            cell.publish(Arc::new(make(g)));
        }
        done.store(true, Ordering::Relaxed);
        let reader_threads: Vec<_> = readers.into_iter().map(|h| h.join().unwrap()).collect();

        drop(Arc::into_inner(cell).expect("readers are joined"));
        let sites = sites.lock().unwrap();
        let mut ids: Vec<_> = sites.iter().map(|&(g, _)| g).collect();
        ids.sort_unstable();
        assert_eq!(ids, (0..=publishes).collect::<Vec<_>>(), "each freed once");
        assert!(
            sites.iter().all(|(_, t)| !reader_threads.contains(t)),
            "a reader thread freed a published value"
        );
    }

    /// The epoch word's packing is derived from `EPOCHS`, and for the
    /// defaults it is the two bits the cell used when both were constants,
    /// so the default cell's protocol is unchanged by making them parameters.
    ///
    /// Mutation: derive `EPOCH_BITS` as `usize::BITS - EPOCHS.leading_zeros()`
    /// (one bit too many at a power of two). The first assertion fails: 3, not
    /// 2.
    #[test]
    fn the_epoch_packing_is_the_fewest_bits_that_name_every_epoch() {
        assert_eq!(ArcLend::<u8>::EPOCH_BITS, 2, "the default four epochs");
        assert_eq!(ArcLend::<u8, 8, 3>::EPOCH_BITS, 2, "loom's three");
        assert_eq!(ArcLend::<u8, 8, 2>::EPOCH_BITS, 1);
        assert_eq!(ArcLend::<u8, 8, 5>::EPOCH_BITS, 3);
        assert_eq!(ArcLend::<u8, 8, MAX_EPOCHS>::EPOCH_BITS, 6);
        assert_eq!(ArcLend::<u8, 8, 2>::EPOCH_MASK, 0b1);
        assert_eq!(ArcLend::<u8, 8, 2>::COUNT_MASK, usize::MAX >> 1);
    }

    /// The smallest valid cell (one slot, two epochs, a one-bit epoch index)
    /// runs the whole protocol: every second lend overflows, reclamation keeps
    /// up while loans are dropped each iteration, one parked overflow loan
    /// (`EPOCHS - 1` of them) stalls the epoch advance and is reported, and
    /// dropping it recovers.
    ///
    /// Mutation: build `EPOCH_MASK` as `1 << EPOCH_BITS` (dropping the `- 1`).
    /// With one index bit, that reads the count's low bit as the epoch: the
    /// overflow lend indexes epoch 2 of 2 and panics on the first iteration.
    #[test]
    fn a_one_slot_two_epoch_cell_runs_the_whole_protocol() {
        let sites = Arc::new(std::sync::Mutex::new(Vec::new()));
        let make = |id| DropSite {
            id,
            sites: sites.clone(),
        };
        let cell: ArcLend<DropSite, 1, 2> = ArcLend::new_with_params(make(0));

        for id in 1..=50 {
            let slot = cell.lend();
            let over = cell.lend();
            assert_eq!(cell.overflow_readers(), 1);
            cell.publish(Arc::new(make(id)));
            assert_eq!((slot.id, over.id), (id - 1, id - 1));
            drop((slot, over));
            assert!(
                cell.retired_len() <= 1,
                "publish {id}: {}",
                cell.retired_len()
            );
        }
        assert_eq!(cell.epoch_stalls(), 0);

        // Park one overflow loan in the current epoch, publish past it so it
        // is sealed, then once more: the only other epoch is occupied.
        let slot = cell.lend();
        let parked = cell.lend();
        cell.publish(Arc::new(make(100)));
        drop(slot);
        cell.publish(Arc::new(make(101)));
        cell.publish(Arc::new(make(102)));
        assert_eq!(cell.epoch_stalls(), 2, "no free epoch to advance into");
        assert_eq!(parked.id, 50);

        drop(parked);
        cell.publish(Arc::new(make(103)));
        cell.publish(Arc::new(make(104)));
        assert_eq!(cell.epoch_stalls(), 2, "the epoch advances again");
        assert_eq!(cell.retired_len(), 0, "and everything is reclaimed");
        assert_eq!(cell.lend().id, 104);
        let freed = sites.lock().unwrap().len();
        assert_eq!(freed, 50 + 1 + 4, "0..=50 and 100..=103, each once");
    }
}
