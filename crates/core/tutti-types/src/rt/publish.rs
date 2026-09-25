//! Publishing non-scalar state to the audio thread as a *borrow*.
//!
//! [`RtPublish`] is the cell a control thread stores into; [`RtRef`] is the
//! non-`Send`, lifetime-tied handle the audio thread reads back. Scalars use
//! [`Param`](crate::value::Param) instead — this is for a routing table, a meter
//! map, a coefficient set.
//!
//! The hazard it removes is an *owning* read on the audio thread: hold the last
//! reference to a retired value and the callback runs `free` on its `Vec`s
//! inside the block. `ClickSettings::meter` did exactly that through
//! `ArcSwap::load_full`.
//!
//! **A no-alloc test cannot pin this property**, and one that claimed to did
//! not: the hazard is a race between a reader and a publisher, and a
//! single-threaded sampling test has no schedule that exhausts it. The guarantee
//! therefore lives in two places that *can* be checked: [`RtRef`]'s type (no
//! owning handle, checked at compile time), and the reclamation protocol below
//! (no code path on the reader side that frees, checked by the `loom` model in
//! `tests/rt_publish_loom.rs` and by miri over this module's tests).
//!
//! # The protocol
//!
//! The cell holds one `AtomicPtr` to the current value (an `Arc` turned into a
//! raw pointer, so the cell owns one strong count), a fixed array of **reader
//! slots**, a fixed array of **overflow epochs**, and a mutex-guarded
//! **control** block (the retirement list and the epoch bookkeeping) that only
//! the publishing side touches. Every published value carries a sequence
//! number, assigned under the mutex.
//!
//! ## The slot path — what a read normally takes
//!
//! 1. claim a free slot with one CAS per slot tried (`FREE → CLAIMED`);
//! 2. `fence(SeqCst)`;
//! 3. load the current pointer (`Acquire`, pairing with the publisher's swap,
//!    so the value is seen fully built);
//! 4. announce that pointer's address in the slot.
//!
//! Dropping the [`RtRef`] stores `FREE` with `Release`.
//!
//! ## The overflow path — when every slot is taken
//!
//! A read that finds no free slot registers in the **current overflow epoch**
//! instead, with one `fetch_add` on the word that names the epoch (so learning
//! the epoch and registering in it are one atomic step — there is no window in
//! which a reader has read a stale epoch but not yet registered). It then loads
//! the current pointer (`Acquire`) and bumps that epoch's `loaded` counter
//! (`Release`). Dropping the [`RtRef`] bumps the epoch's `exited` counter
//! (`Release`).
//!
//! An overflow reader never says which value it holds, but its epoch bounds
//! it: an epoch pins the contiguous run of sequence numbers that were current
//! from the moment it became current until the publisher *sealed* it. That is
//! what keeps a stuck overflow reader — more than the slot count of overlapping
//! readers, or an `RtRef` passed to `mem::forget` — from stalling reclamation
//! forever: it pins its own epoch's run, a couple of values, and nothing else.
//!
//! ## What a reader does not do
//!
//! Either way, the reader never touches a refcount, never takes the lock, never
//! frees, and never loops: at most `READER_SLOTS` CASes, or one `fetch_add`
//! plus one more. The read is **wait-free** and **allocation-free**.
//!
//! ## A publish, under the control lock
//!
//! 1. reserve room in the retirement list (the only allocation, done before
//!    anything changes, so a panic there leaves the cell untouched);
//! 2. swap the new pointer in (`AcqRel`) and retire the old one with its
//!    sequence number;
//! 3. `fence(SeqCst)`;
//! 4. **advance the overflow epoch**, if an epoch slot is free: open it
//!    (starting at the new value's sequence number), swap it into the epoch
//!    word — which returns how many readers registered in the outgoing epoch —
//!    and seal the outgoing epoch once all of those readers have loaded
//!    (spinning on the control thread; each is a few instructions from done).
//!    The sealed epoch pins `[its start, the new value]`;
//! 5. read every reader slot, spinning past one that is `CLAIMED` (a reader
//!    between claim and announce), and free — after dropping the lock — every
//!    retired value that no slot announces and no live epoch pins. The current
//!    epoch is always live and pins everything from its start on; a sealed
//!    epoch is live until its `exited` count reaches its registrations.
//!
//! A retired value a reader still holds is **not** waited for — a publish that
//! waited on a held read would deadlock a thread that reads and then publishes.
//! It stays on the list and is freed by a later publish, or when the cell drops.
//! Either way the free runs on the control side.
//!
//! # The limit
//!
//! The bound on the retired list rests on a free epoch being available at
//! publish time. Suppose every epoch but the current one is sealed and still
//! has a live reader. For example, three overflow `RtRef`s are parked in three
//! different epochs, which takes more than the slot count of refs held
//! across several publishes. Then `publish` cannot advance: the current epoch
//! stays open, and it pins every value retired from then on. Memory grows,
//! one value per publish, until one of those refs is dropped. Debug builds
//! assert at 1024 retired values. Release builds report it through
//! [`RtPublish::retired_len`] and [`RtPublish::epoch_stalls`], which a host can
//! poll.
//!
//! This is the design's limit, not a bug: reaching it takes long-lived,
//! parked `RtRef`s, which the rules already forbid (read once per block, never
//! hold one across blocks). Nothing is unsound in that state. It only leaks
//! until the refs are dropped.
//!
//! # Why it is sound
//!
//! The argument is in C++20's terms (Rust's atomics model), and each step names
//! the rule it rests on.
//!
//! - **Slot path.** The reader's claim is sequenced before its fence `Fr`, and
//!   its pointer load after; the publisher's swap is sequenced before its fence
//!   `Fp`, and its slot read after. `SeqCst` fences are totally ordered
//!   ([atomics.order]/4). If `Fp` precedes `Fr`, the reader's load sees the
//!   swap or later, so it cannot pick up the value being retired. If `Fr`
//!   precedes `Fp`, the publisher's slot read sees the claim or a later value
//!   of that slot ([atomics.order]/4's fence-to-fence clause) — `CLAIMED`, which
//!   it waits out, or the announced address, which it keeps. A slot seen `FREE`
//!   after a read was released with `Release` and read with `Acquire`, so that
//!   reader's accesses happen before the free.
//! - **Slot reuse.** Reader B's claim CAS reads reader A's `Release` store of
//!   `FREE`. That CAS is `Relaxed`, but B's `SeqCst` fence right after it acts
//!   as an acquire fence for that read and as a release fence for what B does
//!   next, so A's use of the slot happens before B's, and the publisher can
//!   never see the two readers' announcements interleaved out of order.
//! - **Overflow path.** No fence is needed. A reader's registration is an RMW
//!   on the epoch word, and every RMW continues the release sequence headed by
//!   the publisher's swap that opened the epoch ([intro.races]/5), so the
//!   `Acquire` registration synchronizes with that swap: the reader's pointer
//!   load sees the value that opened its epoch or a later one. The publisher's
//!   swap that closes the epoch reads the last registration in the word's
//!   modification order, so its count is exact. Each reader's `Release`
//!   increment of `loaded` after its pointer load, read by the publisher's
//!   `Acquire` spin, puts every epoch reader's load before the seal — which is
//!   why a sealed epoch's upper bound covers what its readers hold. `exited` is
//!   the same pairing for the free.
//!
//! loom checks this against the shipped code (see the model's docs). Its
//! `SeqCst` fences are *stronger* than C++'s — it gives them a total order that
//! also constrains surrounding accesses — so a passing model supports the
//! argument above rather than replacing it; miri's weak-memory emulation over
//! the stress test is the second, independent check.
//!
//! # Where a free can still happen
//!
//! The cell *itself* owns the current value and the retired list. Dropping the
//! last `Arc<RtPublish<_>>` frees them on whichever thread drops it — so an
//! audio node that owns the last handle to a cell will free on the audio
//! thread. Keep a control-side handle alive for as long as a node might hold
//! one, which is what every engine call site does.

use std::marker::PhantomData;
use std::ops::Deref;
use std::ptr::NonNull;
use std::sync::Arc;

use sync::{backoff, fence, AtomicPtr, AtomicUsize, Mutex, Ordering};

/// The atomics the protocol runs on: `std`'s in a real build, `loom`'s under
/// `--cfg loom`, so the model in `tests/rt_publish_loom.rs` checks *this* code
/// rather than a replica of it. (`tutti-shm-model` has to use a replica because
/// its protocol lives in an mmap and its crate's dependency closure breaks under
/// the flag; neither is true here — nothing `tutti-types` depends on reacts to
/// `cfg(loom)`.)
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

    /// One step of a control-thread wait on a reader that is a few
    /// instructions from done. Spins briefly, then yields: a low-priority
    /// reader descheduled mid-read must get the CPU back, or a `SCHED_FIFO`
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

/// How many readers can hold an [`RtRef`] into one cell at once before the
/// next one takes the overflow path.
///
/// Per *cell*, not per thread: nested reads through two different cells use
/// one slot each, in their own cells. Eight covers the engine's real shape (one
/// audio thread, one read per block, the odd `Debug` from a control thread)
/// with room to spare, and fits one cache line. Under loom it is two, so the
/// overflow path is reachable in a model small enough to exhaust.
#[cfg(not(loom))]
const READER_SLOTS: usize = 8;
#[cfg(loom)]
const READER_SLOTS: usize = 2;

/// How many overflow epochs a cell cycles through. One is current; the others
/// are sealed epochs waiting for their readers to leave, or free. With four, a
/// permanently stuck overflow reader (a forgotten `RtRef`) occupies one and the
/// rest keep cycling. Under loom it is three: the fewest at which the seal's
/// wait on `loaded` matters (with two, the other epoch is always current and
/// pins everything newer), so the model can catch its removal.
#[cfg(not(loom))]
const EPOCHS: usize = 4;
#[cfg(loom)]
const EPOCHS: usize = 3;

/// The epoch word packs the current epoch's index into its low bits and the
/// number of readers registered in that epoch above them.
const EPOCH_BITS: u32 = 2;
const EPOCH_MASK: usize = (1 << EPOCH_BITS) - 1;
const _: () = assert!(EPOCHS <= 1 << EPOCH_BITS);
/// Registration counts wrap in the word's upper bits; `loaded` and `exited`
/// are compared against them in the same width.
///
/// Not a no-op, though it looks like one on 64-bit: there the word's count
/// would need 2^62 overflow reads in one epoch to wrap. On a 32-bit target it
/// is 2^30 — about twelve days of one overflow read per millisecond with no
/// publish — and once the word has wrapped, the full-width `loaded` count no
/// longer equals `registered` unless it is masked the same way. The seal's
/// spin would then never end.
const COUNT_MASK: usize = usize::MAX >> EPOCH_BITS;

/// A retired list this long means reclamation has stalled — every epoch pinned
/// by stuck readers, or a slot parked for thousands of publishes. Nothing is
/// unsound at that point, but memory is leaking, so debug builds say so.
/// Release builds report it only through [`RtPublish::retired_len`] and
/// [`RtPublish::epoch_stalls`], for a host to poll; nothing here logs.
const RETIRED_LEAK_THRESHOLD: usize = 1024;

/// Slot value: no reader.
const FREE: usize = 0;
/// Slot value: a reader has claimed the slot but not yet announced what it
/// read. Neither this nor [`FREE`] can collide with an announced address: an
/// `Arc`'s data pointer always points into a real heap block, past the two
/// `usize` counts at its start (even for a zero-sized `T`), so it is never 0
/// or 1.
const CLAIMED: usize = 1;

/// The slots, on their own cache line: readers write them, and keeping them
/// off the line holding `current` means a read does not invalidate the pointer
/// every other reader is about to load.
#[repr(align(64))]
struct Slots([AtomicUsize; READER_SLOTS]);

/// The reader-visible half of one overflow epoch.
struct EpochCounters {
    /// Registrants that have finished loading the pointer.
    loaded: AtomicUsize,
    /// Registrants whose `RtRef` has dropped.
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
struct Control<T> {
    /// Values swapped out but possibly still held, with their sequence number.
    retired: Vec<(Arc<T>, u64)>,
    /// The current value's sequence number.
    seq: u64,
    epochs: [EpochState; EPOCHS],
    /// Publishes that found no free epoch to advance into.
    epoch_stalls: u64,
}

/// A value published from a control thread and read by the audio thread.
///
/// For state too large to pack into an atomic — a routing table, a meter map, a
/// coefficient set. Scalars want [`Param`](crate::value::Param), not this.
///
/// # The invariant
///
/// **The audio thread never holds an owning handle to published state, and
/// never frees it.**
///
/// [`read`](Self::read) hands back an [`RtRef`], which is a *borrow*. Dropping
/// it clears a reader slot or bumps a counter; it does not decrement an `Arc`
/// the way an owning handle would, and there is no code on that path that could
/// run a destructor. That matters because the audio thread is the one place a
/// deallocation must not happen: if the callback held the last reference to a
/// retired value, it would run `free` on its `Vec`s inside the block.
///
/// Retired values are freed by [`publish`](Self::publish), on the publishing
/// thread — or, when a reader still holds one at that moment, by a later
/// publish or by the cell's own drop. The cost is real, but it always lands
/// where blocking is allowed. (The one exception is dropping the cell itself:
/// see the module docs on who should own the last handle.)
///
/// This is structural, not probabilistic. The previous implementation wrapped
/// `arc_swap::ArcSwap`, whose guard could degrade into an owning reference when
/// a writer settled its debt concurrently, making an audio-thread free "very
/// unlikely" rather than impossible. The protocol that replaced it (spelled
/// out at the top of `rt/publish.rs`) has no such path, and the `loom` model
/// in `tests/rt_publish_loom.rs` asserts it.
///
/// There is deliberately no owning read, and no way to get an `Arc` back out.
///
/// # Cost
///
/// A read is one CAS on a slot, a `SeqCst` fence and an `Acquire` load, plus a
/// `Release` store when the [`RtRef`] drops — heavier than a plain atomic load,
/// far lighter than a lock, and **wait-free**. **Read once per block, not per
/// sample.** Every call site in the engine hoists it to block scope.
///
/// # Nested reads
///
/// Each live [`RtRef`] into a cell occupies one of that cell's reader slots.
/// Holding more than the slot count at once (nested reads of the *same* cell,
/// or many threads reading together) is safe: the extra readers take the
/// overflow path, which is just as wait-free and allocation-free. What it costs
/// is precision in reclamation: an overflow reader pins every value that was
/// current during its epoch (usually one or two), not just the one it holds.
/// Prefer one read per block, passed down.
pub struct RtPublish<T> {
    /// The current value, from [`Arc::into_raw`]. The cell owns that one strong
    /// count; [`Drop`] gives it back.
    current: AtomicPtr<T>,
    /// The current overflow epoch's index (low bits) and registration count.
    epoch_word: AtomicUsize,
    /// Per-epoch counters overflow readers bump.
    epochs: [EpochCounters; EPOCHS],
    /// Per-reader hazard slots: `FREE`, `CLAIMED`, or the address of the value
    /// the reader holds.
    slots: Slots,
    /// Touched only by `publish` and `Drop`, never by a reader — that is the
    /// whole design. The mutex also serializes concurrent publishers.
    control: Mutex<Control<T>>,
    /// The cell owns `Arc<T>`s through a raw pointer; this gives it `Arc<T>`'s
    /// auto traits (`Send + Sync` exactly when `T: Send + Sync`) and drop-check
    /// behaviour, so no `unsafe impl` is needed.
    _owns: PhantomData<Arc<T>>,
}

impl<T> RtPublish<T> {
    /// Wraps an initial value, allocating the `Arc` that carries it.
    ///
    /// Control-thread only, like [`publish`](Self::publish).
    pub fn new(value: T) -> Self {
        Self::from_arc(Arc::new(value))
    }

    /// Build from an existing `Arc`, when the publisher already has one.
    ///
    /// The cell takes over that strong count; other clones the caller keeps are
    /// unaffected, and the value is freed wherever its *last* `Arc` drops —
    /// which is never a reader, since readers do not hold one.
    pub fn from_arc(value: Arc<T>) -> Self {
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
                // control side, where growing it is allowed.
                retired: Vec::new(),
                seq: 0,
                epochs,
                epoch_stalls: 0,
            }),
            _owns: PhantomData,
        }
    }

    /// Read the current value. **Audio-thread safe**: wait-free, and it
    /// neither allocates nor frees, whatever the publisher is doing.
    ///
    /// Hold the returned [`RtRef`] for the block you are rendering and no
    /// longer: it is a read lease, and a parked one keeps a retired value alive
    /// until the next [`publish`](Self::publish) after it is dropped.
    #[inline]
    pub fn read(&self) -> RtRef<'_, T> {
        // One CAS per slot tried, never a retry of the same slot, so this is
        // bounded by READER_SLOTS whatever else is running. `Relaxed` is
        // enough: the fence below orders the claim against the publisher's
        // slot scan (see "Slot reuse" in the module docs).
        let slot = self.slots.0.iter().position(|s| {
            s.compare_exchange(FREE, CLAIMED, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        });
        let Some(slot) = slot else {
            return self.read_overflow();
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
        // needs ordered — our reads of the value before its free — is carried
        // by the `Release` in `RtRef::drop`.
        self.slots.0[slot].store(ptr.addr(), Ordering::Relaxed);

        RtRef {
            cell: self,
            value: Self::non_null(ptr),
            held: Held::Slot(slot),
            _not_send: PhantomData,
        }
    }

    /// Every slot is taken: register in the current overflow epoch instead.
    #[cold]
    fn read_overflow(&self) -> RtRef<'_, T> {
        // Learning the epoch and registering in it are one RMW, so there is no
        // stale-epoch window. `Acquire` synchronizes with the swap that opened
        // this epoch (every RMW continues its release sequence), so the load
        // below sees the value the epoch started at, or a later one.
        let word = self
            .epoch_word
            .fetch_add(1 << EPOCH_BITS, Ordering::Acquire);
        let epoch = word & EPOCH_MASK;
        let ptr = self.current.load(Ordering::Acquire);
        // `Release`: our load happens before the publisher's seal reads this,
        // so the sealed range covers whatever we just loaded. (Weakening this
        // to `Relaxed` would let the load read a value published after the
        // seal — load buffering, which loom does not explore, so the model
        // cannot catch that mutation. This ordering is held by the argument
        // in the module docs alone.)
        self.epochs[epoch].loaded.fetch_add(1, Ordering::Release);
        RtRef {
            cell: self,
            value: Self::non_null(ptr),
            held: Held::Epoch(epoch),
            _not_send: PhantomData,
        }
    }

    #[inline]
    fn non_null(ptr: *mut T) -> NonNull<T> {
        // `current` is only ever set from `Arc::into_raw`, which is never null.
        NonNull::new(ptr).expect("RtPublish::current is never null")
    }

    /// Publish a new value. **Control-thread only.**
    ///
    /// Swaps the value in, then frees every retired value no reader can still
    /// hold — including the outgoing one, if nobody is reading it. It may spin
    /// briefly on a reader caught mid-read (a handful of instructions on the
    /// reader's side; the spin backs off to `yield_now`), and it takes a mutex
    /// that serializes publishers. Both costs — the wait and the free — belong
    /// to the caller, which is the entire point: neither reaches the callback.
    ///
    /// A retired value that a reader *is* holding is not waited for; it stays
    /// retired and is freed by the next publish after the reader lets go, or
    /// when the cell drops.
    ///
    /// Calling this from the audio thread would stall the callback *and* free
    /// inside it, defeating the type.
    pub fn publish(&self, value: Arc<T>) {
        let freeable = {
            // Nothing below can panic between changing shared state and making
            // the control block consistent again: the only allocations (the
            // two `reserve`s) happen before the swap and before anything is
            // moved out of `retired`. So a poisoned lock — a panic in a
            // `reserve`, or in a previous publisher's `T::drop`, which runs
            // outside the lock anyway — still guards a consistent block, and
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

            debug_assert!(
                control.retired.len() < RETIRED_LEAK_THRESHOLD,
                "RtPublish: {} retired values are still pinned — a reader \
                 parked across thousands of publishes, or every overflow epoch \
                 held by a forgotten RtRef",
                control.retired.len()
            );
            freeable
        };
        // Freed outside the lock: `T::drop` is arbitrary code, and one that
        // published to this cell (or panicked) must not do it under our mutex.
        drop(freeable);
    }

    /// Open a free epoch and seal the current one, if a free epoch exists.
    ///
    /// If none does — every other epoch still has readers in it — the current
    /// epoch stays open and keeps pinning everything from its start, until a
    /// later publish finds one free.
    fn advance_epoch(&self, control: &mut Control<T>) {
        let free = (0..EPOCHS).find(|&k| match control.epochs[k] {
            EpochState::Free => true,
            EpochState::Current { .. } => false,
            EpochState::Sealed { registered, .. } => {
                // `Acquire` pairs with each reader's `Release` exit.
                let exited = self.epochs[k].exited.load(Ordering::Acquire);
                exited & COUNT_MASK == registered
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
        let closing = word & EPOCH_MASK;
        let registered = word >> EPOCH_BITS;
        let EpochState::Current { start } = control.epochs[closing] else {
            unreachable!("the epoch word always names the current epoch");
        };

        // Wait for the closing epoch's readers to finish loading, so the seal
        // bounds what they can hold. Each is past its registration and a load
        // away from done.
        let mut spins = 0;
        while self.epochs[closing].loaded.load(Ordering::Acquire) & COUNT_MASK != registered {
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
    /// drops — nothing here can unwind with a value half-removed.
    fn take_unprotected(&self, control: &mut Control<T>, freeable: &mut Vec<Arc<T>>) {
        let mut held = [FREE; READER_SLOTS];
        for (held, slot) in held.iter_mut().zip(&self.slots.0) {
            // `Acquire`, pairing with `RtRef::drop`'s `Release`: a slot seen
            // `FREE` after a read means that reader's accesses are done.
            let mut seen = slot.load(Ordering::Acquire);
            // A reader between claim and announce. It is running a fence and
            // a load, not waiting on anything, so this ends as soon as it is
            // scheduled — and the waiting is on the control thread.
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
                        && self.epochs[k].exited.load(Ordering::Acquire) & COUNT_MASK != registered
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

    /// How many readers currently hold an [`RtRef`] on the overflow path.
    #[cfg(all(test, not(loom)))]
    fn overflow_readers(&self) -> usize {
        let control = self.control.lock().unwrap();
        (0..EPOCHS)
            .map(|k| {
                let exited = self.epochs[k].exited.load(Ordering::SeqCst);
                match control.epochs[k] {
                    EpochState::Free => 0,
                    EpochState::Current { .. } => {
                        (self.epoch_word.load(Ordering::SeqCst) >> EPOCH_BITS) - exited
                    }
                    EpochState::Sealed { registered, .. } => registered - exited,
                }
            })
            .sum()
    }

    /// How many swapped-out values are still waiting to be freed.
    /// **Control-thread only**: it takes the publisher's lock.
    ///
    /// For a host to poll. It should stay small: a few values plus one per
    /// slot reader parked across publishes. If it keeps growing, reclamation
    /// has stalled — see "The limit" in the module docs.
    pub fn retired_len(&self) -> usize {
        self.control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retired
            .len()
    }

    /// How many publishes found every overflow epoch still occupied, and so
    /// could not start a new one. **Control-thread only**: it takes the
    /// publisher's lock.
    ///
    /// A counter rather than a log line, for a host to poll. It stays at zero
    /// while `RtRef`s are held only within a block. It rises when several
    /// long-lived overflow `RtRef`s occupy every epoch, and while that lasts,
    /// every retired value stays pinned (module docs, "The limit").
    pub fn epoch_stalls(&self) -> u64 {
        self.control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .epoch_stalls
    }
}

impl<T> Drop for RtPublish<T> {
    fn drop(&mut self) {
        // `&mut self` means no `RtRef` is alive (each borrows the cell), so
        // nothing is protected, everything can go, and no atomic access is
        // needed.
        #[cfg(not(loom))]
        let current = *self.current.get_mut();
        #[cfg(loom)]
        let current = self.current.with_mut(|p| *p);
        // SAFETY: as in `publish` — `current` holds a pointer from
        // `Arc::into_raw` whose count the cell owns, and the cell is being
        // destroyed, so nothing will read `current` again.
        drop(unsafe { Arc::from_raw(current) });
        // The retired list's `Arc`s drop with the mutex, after this body.
    }
}

impl<T: Default> Default for RtPublish<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for RtPublish<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RtPublish").field(&*self.read()).finish()
    }
}

/// Which registration an [`RtRef`] must undo.
#[derive(Clone, Copy)]
enum Held {
    Slot(usize),
    Epoch(usize),
}

/// A borrow of the value inside an [`RtPublish`], valid for as long as it is
/// held.
///
/// Deliberately not `Send`, and tied to the cell's lifetime. Between them, a
/// guard cannot be stashed in a struct field, moved to another thread, or
/// outlive the cell — so "held across blocks", the failure mode a review rule
/// would otherwise have to catch, is a compile error instead.
///
/// There is no way to get an owning `Arc` back out. That is not an oversight.
/// Dropping one clears a slot or bumps a counter and does nothing else: it
/// cannot free.
pub struct RtRef<'a, T> {
    cell: &'a RtPublish<T>,
    /// The value this reader loaded. Kept alive by its registration, not by a
    /// refcount.
    value: NonNull<T>,
    held: Held,
    /// `*const ()` is neither `Send` nor `Sync`, which is what pins the guard
    /// to the thread that took it. (`NonNull` alone would do the same; this
    /// says so on purpose rather than by accident of representation.)
    _not_send: PhantomData<*const ()>,
}

impl<T> Deref for RtRef<'_, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: `value` was loaded from `current` after this reader
        // registered, and the registration stays in place until `drop`. On the
        // slot path, a publisher frees a retired value only after its fence
        // finds no slot announcing it, and the fence pair means it cannot miss
        // this slot while having retired the value this reader loaded. On the
        // overflow path, the reader's epoch stays live until `drop`, and a
        // live epoch pins every value its readers can have loaded (module
        // docs, "Why it is sound"). The `Acquire` load makes the pointee's
        // construction visible. The borrow is tied to `&self`, which cannot
        // outlive the registration.
        unsafe { self.value.as_ref() }
    }
}

impl<T> Drop for RtRef<'_, T> {
    #[inline]
    fn drop(&mut self) {
        // `Release`: every read this thread made through the ref happens-before
        // a publisher's `Acquire` of the cleared slot or the exit count, and
        // therefore before the free. Nothing here can run a destructor.
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

impl<T: std::fmt::Debug> std::fmt::Debug for RtRef<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        (**self).fmt(f)
    }
}

// Under `--cfg loom` the atomics above are loom's, which panic outside a
// `loom::model`, so these ordinary tests are compiled out; the loom model is
// `tests/rt_publish_loom.rs`.
#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn read_sees_the_published_value() {
        let cell = RtPublish::new(vec![1u32, 2, 3]);
        assert_eq!(&*cell.read(), &[1, 2, 3]);

        cell.publish(Arc::new(vec![4, 5]));
        assert_eq!(&*cell.read(), &[4, 5]);
    }

    #[test]
    fn a_held_read_keeps_seeing_its_own_snapshot() {
        // The block-scoped contract in miniature: a reader that took a value
        // before a publish keeps rendering with it, rather than tearing
        // mid-block.
        let cell = RtPublish::new(vec![1u32]);
        let held = cell.read();

        cell.publish(Arc::new(vec![9]));

        assert_eq!(&*held, &[1], "guard still sees the snapshot it took");
        assert_eq!(&*cell.read(), &[9], "a fresh read sees the new value");
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
        let cell = RtPublish::new(Tattle(dropped.clone()));

        assert!(!dropped.load(Ordering::SeqCst));
        cell.publish(Arc::new(Tattle(Arc::new(AtomicBool::new(false)))));
        assert!(
            dropped.load(Ordering::SeqCst),
            "retired value must be freed by publish(), on the publishing thread"
        );
    }

    /// A guard must not be able to escape the thread that took it — that is what
    /// makes "held across blocks" a compile error rather than a review rule.
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
    /// *build*. That matches what it guards: `RtRef`'s non-`Send`ness is a
    /// type-system claim, and putting the guarantee in the return type is what
    /// stops it depending on anyone remembering to check.
    #[test]
    fn rt_ref_is_not_send() {
        struct Wrap<T>(PhantomData<T>);

        trait NotSend {
            const IS_SEND: bool = false;
        }
        impl<T> NotSend for Wrap<T> {}

        impl<T: Send> Wrap<T> {
            const IS_SEND: bool = true;
        }

        // A guard that escapes its thread outlives the block it was taken for.
        const {
            assert!(!<Wrap<RtRef<'static, Vec<u32>>>>::IS_SEND);
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
    /// `Arc<RtPublish<_>>` across threads. The raw pointer inside would make it
    /// neither `Send` nor `Sync` by default; `PhantomData<Arc<T>>` plus the
    /// atomics is what gives it `Arc<T>`'s auto traits back without an
    /// `unsafe impl`.
    ///
    /// Mutation: replace `_owns: PhantomData<Arc<T>>` with
    /// `PhantomData<*const T>` — this stops compiling.
    #[test]
    fn rt_publish_is_send_and_sync_for_send_sync_payloads() {
        fn assert_send_sync<X: Send + Sync>() {}
        assert_send_sync::<RtPublish<Vec<u32>>>();
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
    /// property the arc-swap implementation could not promise: there, the last
    /// holder of a settled debt ran the destructor wherever it was.
    ///
    /// Mutation: make `RtRef::drop` free the retired list (call
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
        let cell = Arc::new(RtPublish::new(make(0)));

        let (took_tx, took_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let reader = {
            let cell = cell.clone();
            std::thread::spawn(move || {
                let held = cell.read();
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
    fn nested_reads_past_the_slot_count_overflow_safely() {
        let sites = Arc::new(std::sync::Mutex::new(Vec::new()));
        let make = |id| DropSite {
            id,
            sites: sites.clone(),
        };
        let here = std::thread::current().id();
        let cell = RtPublish::new(make(0));

        let in_slots: Vec<_> = (0..READER_SLOTS).map(|_| cell.read()).collect();
        assert_eq!(cell.overflow_readers(), 0, "these fit in the slots");
        cell.publish(Arc::new(make(1)));

        let overflowed: Vec<_> = (0..2).map(|_| cell.read()).collect();
        assert_eq!(cell.overflow_readers(), 2, "two reads past the slots");
        assert!(overflowed.iter().all(|r| r.id == 1));

        cell.publish(Arc::new(make(2)));
        assert!(
            sites.lock().unwrap().is_empty(),
            "value 1 is pinned by the overflow readers' sealed epoch"
        );
        // A read taken now sees the newest value even while older ones are
        // held (on the overflow path too — every slot is still taken).
        assert_eq!(cell.read().id, 2);
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
        let r = cell.read();
        assert_eq!(r.id, 4);
        assert_eq!(cell.overflow_readers(), 0);
    }

    /// An overflow `RtRef` passed to `mem::forget` never exits its epoch, so
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
        let cell = RtPublish::new(make(0));

        let in_slots: Vec<_> = (0..READER_SLOTS).map(|_| cell.read()).collect();
        let forgotten = cell.read();
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
        let cell = RtPublish::new(0u32);
        let in_slots: Vec<_> = (0..READER_SLOTS).map(|_| cell.read()).collect();
        let over = cell.read();
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
            assert_eq!(*cell.read(), v);
        }
    }

    /// The design's limit, and its release-mode observables: park an
    /// overflow `RtRef` in every epoch but the current one and the current
    /// epoch can no longer advance. Each publish counts a stall, and the
    /// retired list grows. Dropping the parked refs ends it: the next publish
    /// advances again and reclaims everything.
    ///
    /// Mutation: delete `control.epoch_stalls += 1` in `advance_epoch`. The
    /// stall count stays 0 and the first stall assertion fails.
    #[test]
    fn every_epoch_parked_is_observable_and_recovers() {
        let cell = RtPublish::new(0u32);
        let in_slots: Vec<_> = (0..READER_SLOTS).map(|_| cell.read()).collect();
        // One overflow ref per epoch but the last: each publish seals the
        // epoch the previous read landed in and opens the next.
        let mut parked = Vec::new();
        for v in 1..EPOCHS as u32 {
            parked.push(cell.read());
            cell.publish(Arc::new(v));
        }
        assert_eq!(
            cell.epoch_stalls(),
            0,
            "every advance so far found a free epoch"
        );
        parked.push(cell.read()); // the current epoch is occupied too

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

        let cell = RtPublish::new(Bomb(true));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cell.publish(Arc::new(Bomb(false)));
        }));
        assert!(result.is_err(), "the retired value's drop panicked");
        assert!(!cell.control.is_poisoned());
        assert_eq!(cell.retired_len(), 0, "it left the list before it ran");

        cell.publish(Arc::new(Bomb(false)));
        assert!(!cell.read().0);
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
        let cell = RtPublish::new(make(0));

        let held = cell.read();
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
        let cell = Arc::new(RtPublish::new(make(0)));
        let done = Arc::new(AtomicBool::new(false));

        let readers: Vec<_> = (0..READERS)
            .map(|_| {
                let (cell, done) = (cell.clone(), done.clone());
                std::thread::spawn(move || {
                    let mut last = 0;
                    let mut reads = 0usize;
                    while !done.load(Ordering::Relaxed) || reads == 0 {
                        let r = cell.read();
                        let g = r.v[0];
                        assert_ne!(g, POISON, "read a freed value");
                        assert!(r.v.iter().all(|&x| x == g), "torn value {:?}", r.v);
                        assert!(g >= last, "went back in time: {g} after {last}");
                        last = g;
                        reads += 1;
                        // Occasionally nest past the slot count too.
                        if reads.is_multiple_of(64) {
                            let nested: Vec<_> = (0..READER_SLOTS).map(|_| cell.read()).collect();
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
}
