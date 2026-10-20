//! The cell, [`ArcLend`], its registered [`Reader`], and the borrow a reader
//! takes, [`Loan`].
//!
//! The protocol and the argument that it is sound are in
//! [`docs::internal`](crate::docs::internal); this file's comments name the
//! step each line implements.

use std::cell::Cell;
use std::marker::PhantomData;
use std::ops::Deref;
use std::ptr::NonNull;
use std::sync::Arc;

use sync::{fence, AtomicBool, AtomicPtr, AtomicUsize, Mutex, MutexGuard, Ordering};

/// The atomics the protocol runs on: `std`'s in a real build, `loom`'s under
/// `--cfg loom`, so the model in `tests/loom.rs` checks *this* code rather
/// than a replica of it.
mod sync {
    #[cfg(loom)]
    pub(super) use loom::{
        sync::atomic::{fence, AtomicBool, AtomicPtr, AtomicUsize, Ordering},
        sync::{Mutex, MutexGuard},
    };
    #[cfg(not(loom))]
    pub(super) use std::{
        sync::atomic::{fence, AtomicBool, AtomicPtr, AtomicUsize, Ordering},
        sync::{Mutex, MutexGuard},
    };
}

/// Reader slots per block of the slot list. The list grows a block at a time
/// and never shrinks or moves, so a [`Reader`] can keep a pointer to its slot.
///
/// Under `--cfg loom` it is two, so a model with three readers grows the list.
#[cfg(not(loom))]
const BLOCK: usize = 16;
#[cfg(loom)]
const BLOCK: usize = 2;

/// Slot word: no loan. Every other value is either a *claim* (odd: the
/// reader's generation, shifted left, with the low bit set) or an
/// *announcement* (even and nonzero: the address of the value the reader
/// holds). An `Arc`'s data pointer is never 0 and always even: it points past
/// the two `usize` counts at the start of a heap block that is itself aligned
/// to at least a `usize`, so it cannot collide with either.
const FREE: usize = 0;

/// One cache line (two on x86-64 and aarch64, whose adjacent-line prefetcher
/// pulls lines in pairs; the same choice as `crossbeam-utils`' `CachePadded`).
/// Each reader's slot and the `current` pointer get one of their own, so a
/// lend writes a line no other reader reads or writes.
#[cfg_attr(any(target_arch = "x86_64", target_arch = "aarch64"), repr(align(128)))]
#[cfg_attr(
    not(any(target_arch = "x86_64", target_arch = "aarch64")),
    repr(align(64))
)]
struct Padded<T>(T);

/// One registered reader's slot.
struct Slot {
    /// [`FREE`], a claim, or an announcement. Written only by the [`Reader`]
    /// that owns the slot (and reset to `FREE` by the registration that hands
    /// the slot to a new reader, under the control lock).
    word: AtomicUsize,
    /// Whether a live [`Reader`] owns the slot. Set by registration, under the
    /// control lock; cleared by the reader's drop, with one store.
    owned: AtomicBool,
}

/// A fixed-size run of slots, allocated once and freed with the cell.
struct Block([Padded<Slot>; BLOCK]);

impl Block {
    fn new() -> Self {
        Self(std::array::from_fn(|_| {
            Padded(Slot {
                word: AtomicUsize::new(FREE),
                owned: AtomicBool::new(false),
            })
        }))
    }
}

/// What the publisher last saw in a slot that held a claim: the claim word
/// (which carries the reader's generation) and the oldest sequence number the
/// claiming reader can load.
#[derive(Clone, Copy)]
struct Claim {
    word: usize,
    floor: u64,
}

/// Everything only the publishing side touches.
struct Control<T> {
    /// Values swapped out but possibly still held, oldest first, with the
    /// sequence number each had while it was current.
    retired: Vec<(Arc<T>, u64)>,
    /// The current value's sequence number.
    seq: u64,
    /// `seq` as of the last scan of the slots.
    scanned_at: u64,
    /// The slot list. Append-only; a block's address never changes, and
    /// blocks are freed only with the cell.
    blocks: Vec<Arc<Block>>,
    /// Per slot, the claim the last scan saw there, if it saw one.
    claims: Vec<Option<Claim>>,
    /// Where the next registration starts looking for an unowned slot.
    hint: usize,
    /// The scan's buffer of announced addresses, reused across publishes.
    /// Its capacity is kept at the slot count, so a scan never grows it.
    announced: Vec<usize>,
    /// The buffer retired values are moved into before they are freed
    /// (outside the lock), reused across publishes.
    spare: Vec<Arc<T>>,
}

/// A value one side publishes and registered [`Reader`]s only ever *borrow*.
///
/// For state too large to pack into an atomic: a routing table, a config
/// snapshot, a coefficient set, a level's collision mesh.
///
/// # The invariant
///
/// **A reader never holds an owning handle to published state, and never
/// frees it.**
///
/// [`Reader::lend`] hands back a [`Loan`], which is a *borrow*. Dropping it
/// clears the reader's slot with one store; it does not decrement an `Arc`
/// the way an owning handle would, and there is no code on that path that
/// could run a destructor. That matters when the reader is a thread on which
/// a deallocation must not happen (an audio callback, a game's frame tick, a
/// control loop with a deadline): if it held the last reference to a retired
/// value, it would run `free` inside its deadline.
///
/// Retired values are freed by [`publish`](Self::publish) or
/// [`reclaim`](Self::reclaim), on the publishing thread, or, when a reader
/// still holds one at that moment, by a later publish or reclaim, or by the
/// cell's own drop. The cost is real, but it always lands where blocking is
/// allowed.
///
/// This is structural, not probabilistic: no code path on the reader side can
/// free, whatever the interleaving with a publisher.
///
/// There is deliberately no owning read, and no way to get an `Arc` back out.
///
/// # Readers
///
/// A thread that reads the cell first registers a [`Reader`] with
/// [`reader`](Self::reader), on the control side, where registering may
/// allocate. The reader owns one slot of the cell's slot list, on a cache
/// line of its own, and every lend writes only that slot. Hold one reader per
/// thread, and one more per level of nesting: [`Reader::lend`] takes
/// `&mut self`, so a reader has at most one live loan.
///
/// # Who drops the cell
///
/// The cell owns the current value and the retired list. Every [`Reader`]
/// holds an `Arc` of the cell, so the cell is freed (with those values) where
/// the last `Arc` drops, which can be a reader's. Keep a publisher-side
/// handle alive for as long as any reader exists.
///
/// # Cost
///
/// A lend is two stores to the reader's own slot, a `SeqCst` fence and an
/// `Acquire` load, plus a `Release` store when the [`Loan`] drops: heavier
/// than a plain atomic load, far lighter than a lock, and **wait-free**.
/// **Lend once per iteration of the reader's loop, not per element.** See the
/// crate docs' cost table.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use arc_lend::ArcLend;
///
/// // Publisher: build and share the cell, and register a reader for the
/// // deadline thread (registering may allocate; lending never does).
/// let routing = Arc::new(ArcLend::new(vec![0usize, 1]));
/// let mut reader = routing.reader();
///
/// // Reader, once per iteration: a borrow that cannot free.
/// {
///     let table = reader.lend();
///     assert_eq!(table[1], 1);
/// }
///
/// // Publisher: swap in a new table; the old one is freed here.
/// routing.publish(Arc::new(vec![1, 0]));
/// assert_eq!(reader.lend()[0], 1);
/// ```
pub struct ArcLend<T> {
    /// The current value, from [`Arc::into_raw`]. The cell owns that one strong
    /// count; [`Drop`] gives it back. On a line of its own: every lend reads
    /// it, and only a publish writes it.
    current: Padded<AtomicPtr<T>>,
    /// Touched only by the publishing side ([`publish`](Self::publish),
    /// [`reclaim`](Self::reclaim), registration, `Drop`), never by a lend or a
    /// loan's drop: that is the whole design. The mutex also serializes
    /// concurrent publishers.
    control: Mutex<Control<T>>,
    /// The cell owns `Arc<T>`s through a raw pointer; this gives it `Arc<T>`'s
    /// auto traits (`Send + Sync` exactly when `T: Send + Sync`) and drop-check
    /// behaviour, so no `unsafe impl` is needed.
    _owns: PhantomData<Arc<T>>,
}

impl<T> ArcLend<T> {
    /// Wraps an initial value, allocating the `Arc` that carries it.
    ///
    /// Publisher side, like [`publish`](Self::publish).
    pub fn new(value: T) -> Self {
        Self::from_arc(Arc::new(value))
    }

    /// Builds the cell from an existing `Arc`, when the publisher already has
    /// one.
    ///
    /// The cell takes over that strong count; other clones the caller keeps are
    /// unaffected, and the value is freed wherever its *last* `Arc` drops,
    /// which is never a reader, since readers do not hold one.
    pub fn from_arc(value: Arc<T>) -> Self {
        Self {
            current: Padded(AtomicPtr::new(Arc::into_raw(value).cast_mut())),
            control: Mutex::new(Control {
                // No capacity reserved: these are only ever touched on the
                // publisher side, where growing them is allowed.
                retired: Vec::new(),
                seq: 0,
                scanned_at: 0,
                blocks: Vec::new(),
                claims: Vec::new(),
                hint: 0,
                announced: Vec::new(),
                spare: Vec::new(),
            }),
            _owns: PhantomData,
        }
    }

    /// The control block. A poisoned lock still guards a consistent block:
    /// nothing that runs under it can panic between changing shared state and
    /// making the block consistent again (the only allocations happen before
    /// the swap, and destructors run after the lock is released), so carrying
    /// on is correct.
    fn lock(&self) -> MutexGuard<'_, Control<T>> {
        self.control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Registers a [`Reader`]. **Control side**: it takes the publisher's
    /// lock, and may allocate (a new block of slots when every slot is
    /// owned). It cannot fail.
    ///
    /// The reader owns one slot until it drops; a dropped reader's slot is
    /// reused by the next registration. The slot list keeps its high-water
    /// mark (see [`docs::limitations`](crate::docs::limitations)).
    ///
    /// The reader holds an `Arc` of the cell, so it can be stored in a
    /// long-lived structure and moved to the thread that will lend.
    pub fn reader(self: &Arc<Self>) -> Reader<T> {
        let slot = self.lock().register();
        Reader {
            cell: Arc::clone(self),
            slot,
            generation: 0,
            _not_sync: PhantomData,
        }
    }

    /// Publishes a new value. **Publisher side only**: never call it from a
    /// thread that must not block or free.
    ///
    /// Swaps the value in, then frees every retired value no reader can still
    /// hold, including the outgoing one if nobody is reading it. It **never
    /// waits on a reader**: a retired value a reader holds, or might be about
    /// to load, stays retired and is checked again by the next publish or
    /// [`reclaim`](Self::reclaim). It takes a mutex that serializes
    /// publishers, and runs the frees after releasing it. Both costs (the
    /// scan and the free) belong to the caller, which is the entire point:
    /// neither reaches a reader.
    pub fn publish(&self, value: Arc<T>) {
        let freeable = {
            let mut control = self.lock();
            let control = &mut *control;

            // Step 1. The only allocations, before anything changes. If one
            // panics, nothing has; `value` is dropped on this thread by the
            // unwind, and no reader has seen it.
            control.retired.reserve(1);
            let mut freeable = std::mem::take(&mut control.spare);
            freeable.reserve(control.retired.len() + 1);

            // Step 2. `Release` publishes the new value's contents to readers'
            // `Acquire` loads; `Acquire` makes the outgoing value's contents
            // ours before we drop it.
            let old_seq = control.seq;
            control.seq += 1;
            let old = self
                .current
                .0
                .swap(Arc::into_raw(value).cast_mut(), Ordering::AcqRel);
            // SAFETY: `old` came out of `current`, which only ever holds a
            // pointer from `Arc::into_raw` whose strong count the cell owns.
            // The swap removed it from `current`, so this is the one place that
            // count is reclaimed. The push cannot allocate (reserved above).
            control
                .retired
                .push((unsafe { Arc::from_raw(old) }, old_seq));

            // Steps 3 and 4.
            self.scan(control, &mut freeable);
            freeable
        };
        self.free(freeable);
    }

    /// Frees every retired value no reader can still hold, without publishing.
    /// **Publisher side only**, like [`publish`](Self::publish), and like it,
    /// never waits on a reader.
    ///
    /// Returns how many retired values are still pinned afterwards: held by a
    /// loan, or possibly about to be loaded by a reader caught mid-lend. A
    /// host that wants the memory back without a new value calls this from a
    /// periodic task; what to do when the count stays high is its policy.
    pub fn reclaim(&self) -> usize {
        let (freeable, pinned) = {
            let mut control = self.lock();
            let control = &mut *control;
            let mut freeable = std::mem::take(&mut control.spare);
            freeable.reserve(control.retired.len());
            self.scan(control, &mut freeable);
            (freeable, control.retired.len())
        };
        self.free(freeable);
        pinned
    }

    /// Steps 3 and 4 of a publish (and all of a reclaim): the publisher half
    /// of the fence pair, then one pass over the slots and one over the
    /// retired list. Moves into `freeable` (which has room for all of
    /// `retired`) every retired value no reader can be holding. Neither
    /// allocates nor drops, so nothing here can unwind with a value
    /// half-moved.
    fn scan(&self, control: &mut Control<T>, freeable: &mut Vec<Arc<T>>) {
        // Step 3: the publisher half of the fence pair. Orders the swap (or,
        // for a reclaim, the last publish's swap) against every reader's
        // claim; see "Why it is sound" in `docs::internal`.
        fence(Ordering::SeqCst);

        // Step 4a: read each slot once.
        let Control {
            retired,
            seq,
            scanned_at,
            blocks,
            claims,
            announced,
            ..
        } = control;
        announced.clear();
        // The oldest sequence number a reader caught mid-lend can load.
        let mut floor = u64::MAX;
        let slots = blocks.iter().flat_map(|b| b.0.iter());
        for (slot, claim) in slots.zip(claims.iter_mut()) {
            // `Acquire`, pairing with the reader's `Release` stores (and the
            // `Release` half of its fence), so whatever the reader did through
            // an earlier loan happens before anything this scan frees.
            let word = slot.0.word.load(Ordering::Acquire);
            if word == FREE {
                *claim = None;
            } else if word & 1 == 1 {
                // A reader between claim and announce. The publisher does not
                // wait for it. If the last scan saw this very claim (same
                // word, so same generation), the reader has been mid-lend
                // since then and keeps the floor recorded then. Otherwise it
                // claimed after the last scan read this slot, so it loads a
                // value no older than the one current at that scan.
                let since = match *claim {
                    Some(seen) if seen.word == word => seen.floor,
                    _ => *scanned_at,
                };
                *claim = Some(Claim { word, floor: since });
                floor = floor.min(since);
            } else {
                *claim = None;
                // Cannot allocate: its capacity is the slot count.
                announced.push(word);
            }
        }
        *scanned_at = *seq;
        announced.sort_unstable();

        // Step 4b: one order-preserving pass over the retired list, so values
        // are freed oldest first. `extract_if` moves each one at most once;
        // `freeable` has room for the whole list.
        let unpinned = retired.extract_if(.., |(value, seq)| {
            *seq < floor && announced.binary_search(&Arc::as_ptr(value).addr()).is_err()
        });
        freeable.extend(unpinned.map(|(value, _)| value));
    }

    /// Runs the frees, outside the lock (`T::drop` is arbitrary code, and one
    /// that published to this cell, or panicked, must not do it under our
    /// mutex), then hands the emptied buffer back for the next publish.
    fn free(&self, mut freeable: Vec<Arc<T>>) {
        freeable.clear();
        // Never waits: if another publisher holds the lock, this buffer is
        // dropped instead and the next publish allocates one.
        if let Ok(mut control) = self.control.try_lock() {
            if control.spare.capacity() < freeable.capacity() {
                control.spare = freeable;
            }
        }
    }

    /// Returns how many swapped-out values are still waiting to be freed.
    ///
    /// **Publisher side**: it takes the publisher's lock.
    ///
    /// For the publisher to poll. At most one value per registered [`Reader`]
    /// is held by a loan; beyond that, a reader preempted between claiming its
    /// slot and announcing pins what was retired while it was preempted, until
    /// the next publish or [`reclaim`](Self::reclaim) after it announces. What
    /// to do about a count that stays high (log, assert in a debug build) is
    /// the caller's policy, which is why the cell itself neither asserts nor
    /// logs.
    pub fn retired_len(&self) -> usize {
        self.lock().retired.len()
    }

    /// How many slots the slot list has: the high-water mark of readers
    /// registered at once, rounded up to a block.
    #[cfg(all(test, not(loom)))]
    fn slot_count(&self) -> usize {
        self.lock().claims.len()
    }
}

impl<T> Control<T> {
    /// Hands an unowned slot to a new reader, growing the list by a block if
    /// every slot is owned.
    fn register(&mut self) -> NonNull<Slot> {
        let len = self.claims.len();
        for i in (self.hint..len).chain(0..self.hint) {
            let slot = &self.blocks[i / BLOCK].0[i % BLOCK].0;
            // `Acquire`, pairing with the previous owner's `Release` in
            // `Reader::drop`: its last store to the slot happens before the
            // new owner's first.
            if !slot.owned.load(Ordering::Acquire) {
                return self.take(i);
            }
        }
        self.blocks.push(Arc::new(Block::new()));
        self.claims.resize(len + BLOCK, None);
        // Keep the scan's buffer able to hold one address per slot.
        self.announced.reserve(len + BLOCK - self.announced.len());
        self.take(len)
    }

    fn take(&mut self, i: usize) -> NonNull<Slot> {
        let slot = &self.blocks[i / BLOCK].0[i % BLOCK].0;
        // Only registration sets `owned`, and only under the lock.
        slot.owned.store(true, Ordering::Relaxed);
        // A previous owner that `mem::forget`ed a loan left its announcement
        // here; the new owner starts from nothing. (Read only under the lock,
        // and by the new owner, which registration happens before.)
        slot.word.store(FREE, Ordering::Relaxed);
        self.hint = i + 1;
        NonNull::from(slot)
    }
}

impl<T> Drop for ArcLend<T> {
    fn drop(&mut self) {
        // `&mut self` means no `Reader` is alive (each holds an `Arc` of the
        // cell), so no loan is, nothing is protected, everything can go, and
        // no atomic access is needed.
        #[cfg(not(loom))]
        let current = *self.current.0.get_mut();
        #[cfg(loom)]
        let current = self.current.0.with_mut(|p| *p);
        // SAFETY: as in `publish`: `current` holds a pointer from
        // `Arc::into_raw` whose count the cell owns, and the cell is being
        // destroyed, so nothing will read `current` again.
        drop(unsafe { Arc::from_raw(current) });
        // The retired list's `Arc`s and the slot blocks drop with the mutex,
        // after this body.
    }
}

impl<T: Default> Default for ArcLend<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for ArcLend<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Read on the publisher side: under the lock, no publish can retire
        // (let alone free) the current value.
        let _control = self.lock();
        let ptr = self.current.0.load(Ordering::Acquire);
        // SAFETY: `current` always holds a live `Arc` the cell owns, and only
        // a publish, which needs the lock held here, can replace it.
        let value = unsafe { &*ptr };
        f.debug_tuple("ArcLend").field(value).finish()
    }
}

/// A registered reader of one [`ArcLend`]: the only way to lend its value.
///
/// Made by [`ArcLend::reader`] on the control side; used on the thread with
/// the deadline. It owns one slot of the cell's slot list, and
/// [`lend`](Self::lend) writes only that slot: no CAS, no scan of other
/// readers' slots, no retry.
///
/// - **`Send`, not `Sync`.** Move it to the thread that reads. It lends
///   through `&mut self`, so one reader has at most one live [`Loan`]: a
///   thread that nests reads of one cell holds one reader per level.
/// - **`Clone` registers a new reader**, with a slot of its own, so it takes
///   the publisher's lock and may allocate: clone on the control side. A
///   clone shares nothing with the original but the cell.
/// - **Dropping it** frees its slot with one store, for the next registration
///   to reuse, and drops its `Arc` of the cell (which frees the cell if it was
///   the last; see [`ArcLend`], "Who drops the cell").
///
/// It is `!Sync`:
///
/// ```compile_fail
/// # // Mutation: `unsafe impl<T: Send + Sync> Sync for Reader<T> {}`; this
/// # // compiles (and `a_reader_is_send_but_not_sync` fails its const
/// # // assertion).
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
    /// Bumped by every lend and written into the claim, so the publisher can
    /// tell a reader still mid-lend from one that has claimed again.
    generation: usize,
    /// `Cell<()>` is `Send` and not `Sync`, and so (with the `Send` impl
    /// below) is the reader. `NonNull` alone would make it `!Sync` too; this
    /// says so on purpose rather than by accident of representation.
    _not_sync: PhantomData<Cell<()>>,
}

// SAFETY: a `Reader` is an `Arc<ArcLend<T>>` (which is `Send` exactly when
// `T: Send + Sync`), a counter, and a pointer to a slot inside that cell: two
// atomics, owned by this reader and read by the publisher. Moving the reader
// to another thread moves that ownership with it; nothing in the slot is tied
// to the thread that registered it. It stays `!Sync` (see `_not_sync`).
unsafe impl<T: Send + Sync> Send for Reader<T> {}

impl<T> Reader<T> {
    #[inline]
    fn slot(&self) -> &Slot {
        // SAFETY: the slot is in a block owned by `self.cell`'s control block,
        // which never moves or frees a block before the cell drops, and
        // `self.cell` keeps the cell alive.
        unsafe { self.slot.as_ref() }
    }

    /// The claim word for this reader's next lend: its new generation, with
    /// the low bit set. Generations wrap; a wrapped one that matches the claim
    /// the publisher last saw only makes it keep more (see `docs::internal`).
    #[inline]
    fn next_claim(&mut self) -> usize {
        self.generation = self.generation.wrapping_add(1);
        (self.generation << 1) | 1
    }

    /// Lends the current value.
    ///
    /// **Safe on a thread with a deadline**: wait-free, and it neither
    /// allocates nor frees, whatever the publisher is doing.
    ///
    /// Hold the returned [`Loan`] for one iteration of your loop and no
    /// longer: a parked one keeps one retired value alive until the next
    /// publish or reclaim after it drops.
    ///
    /// One live loan per reader; a second lend of the same reader while the
    /// first loan is alive does not compile:
    ///
    /// ```compile_fail
    /// # // Mutation: make `lend` take `&self`; this compiles.
    /// let cell = std::sync::Arc::new(arc_lend::ArcLend::new(1u32));
    /// let mut reader = cell.reader();
    /// let a = reader.lend();
    /// let b = reader.lend(); // a second reader is needed for this
    /// assert_eq!(*a + *b, 2);
    /// ```
    #[inline]
    pub fn lend(&mut self) -> Loan<'_, T> {
        let claim = self.next_claim();
        let slot = self.slot();

        // Claim. `Release`: if a publisher reads this claim, everything this
        // reader did through its previous loan happens before that read, and
        // so before anything the publisher then frees (see "A reader's next
        // claim" in `docs::internal`).
        slot.word.store(claim, Ordering::Release);

        // The reader half of the fence pair. Without it, the load below may be
        // satisfied before the claim is visible, and a publisher can scan an
        // apparently free slot, free the value, and hand this reader a
        // dangling pointer.
        fence(Ordering::SeqCst);

        // `Acquire` pairs with the publisher's `AcqRel` swap, so the pointee is
        // fully constructed from this thread's point of view.
        let ptr = self.cell.current.0.load(Ordering::Acquire);

        // Announce. `Relaxed`: the publisher only compares the address, and
        // the fence above already orders this reader's earlier accesses before
        // any scan that reads this store.
        debug_assert!(ptr.addr() & 1 == 0 && ptr.addr() != FREE);
        slot.word.store(ptr.addr(), Ordering::Relaxed);

        Loan {
            slot,
            // `current` is only ever set from `Arc::into_raw`, never null.
            value: NonNull::new(ptr).expect("ArcLend::current is never null"),
            _not_send: PhantomData,
        }
    }

    /// The cell this reader reads, for publishing to it or registering
    /// another reader from wherever the reader is kept.
    pub fn cell(&self) -> &Arc<ArcLend<T>> {
        &self.cell
    }
}

impl<T> Clone for Reader<T> {
    /// Registers a new reader of the same cell: control side, see
    /// [`ArcLend::reader`].
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

/// A borrow of the value inside an [`ArcLend`], valid for as long as it is
/// held.
///
/// Returned by [`Reader::lend`]. Deliberately not `Send`, and tied to the
/// reader's mutable borrow: a loan cannot be moved to another thread or
/// outlive its reader, and its reader cannot lend again until it drops. Hold
/// it for one iteration of the reader's loop and drop it.
///
/// There is no way to get an owning `Arc` back out. That is not an oversight.
/// Dropping one stores `FREE` in the reader's slot and does nothing else: it
/// cannot free.
///
/// ```compile_fail
/// # // Mutation: `unsafe impl<T: Sync> Send for Loan<'_, T> {}`; this
/// # // compiles (and `a_loan_is_not_send` fails its const assertion).
/// let cell = std::sync::Arc::new(arc_lend::ArcLend::new(1u32));
/// let mut reader = cell.reader();
/// let loan = reader.lend();
/// std::thread::scope(|s| {
///     s.spawn(move || *loan); // `Loan` is not `Send`
/// });
/// ```
pub struct Loan<'a, T> {
    /// The lending reader's slot, which announces `value`.
    slot: &'a Slot,
    /// The value this reader loaded. Kept alive by the announcement, not by a
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
        // retired the value this reader loaded (`docs::internal`, "Why it is
        // sound"). The `Acquire` load makes the pointee's construction
        // visible. The borrow is tied to `&self`, which cannot outlive the
        // loan.
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

// Under `--cfg loom` the atomics above are loom's, which panic outside a
// `loom::model`, so these ordinary tests are compiled out; the loom model is
// `tests/loom.rs`.
#[cfg(all(test, not(loom)))]
mod tests;
