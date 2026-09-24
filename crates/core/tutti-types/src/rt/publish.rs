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
//! raw pointer, so the cell owns one strong count), a small fixed array of
//! **reader slots**, an **overflow counter**, and a mutex-guarded **retirement
//! list** only the publishing side touches.
//!
//! A read:
//!
//! 1. claims a free slot with one CAS per slot tried (`FREE → CLAIMED`), or —
//!    if every slot is taken — increments the overflow counter;
//! 2. issues a `SeqCst` fence;
//! 3. loads the current pointer (`Acquire`, pairing with the publisher's swap,
//!    so the value is seen fully built);
//! 4. announces that pointer's address in its slot.
//!
//! Dropping the [`RtRef`] stores `FREE` back into the slot (or decrements the
//! counter) with `Release`. That is the reader's entire involvement: it never
//! touches a refcount, never takes the lock, never frees, and never loops. The
//! read is **wait-free** — at most `READER_SLOTS` CASes plus one `fetch_add`.
//!
//! A publish, under the retirement lock:
//!
//! 1. swaps the new pointer in and pushes the old one onto the retirement list;
//! 2. issues a `SeqCst` fence;
//! 3. if the overflow counter is non-zero, stops: an overflow reader has not
//!    said which value it holds, so every retired value stays retired;
//! 4. otherwise reads every slot — spinning, on *this* thread, past a slot
//!    that is mid-read (`CLAIMED`) until its reader announces — and frees every
//!    retired value whose address no slot holds, after dropping the lock.
//!
//! The two fences are what make step 4 sound. If the publisher's fence comes
//! first in the single total order of `SeqCst` fences, the reader's load in
//! step 3 sees the swap, so it cannot pick up a value that is already retired;
//! if the reader's fence comes first, the publisher's slot read sees `CLAIMED`
//! or the announced address, so it keeps that value. There is no third case.
//!
//! A retired value a reader still holds is **not** waited for — a publish that
//! waited on a held read would deadlock a thread that reads and then publishes.
//! It stays on the list and is freed by a later publish, or when the cell drops.
//! Either way the free runs on the control side.

use std::marker::PhantomData;
use std::ops::Deref;
use std::ptr::NonNull;
use std::sync::Arc;

use sync::{fence, spin_loop, AtomicPtr, AtomicUsize, Mutex, Ordering};

/// The atomics the protocol runs on: `std`'s in a real build, `loom`'s under
/// `--cfg loom`, so the model in `tests/rt_publish_loom.rs` checks *this* code
/// rather than a replica of it. (`tutti-shm-model` has to use a replica because
/// its protocol lives in an mmap and its crate's dependency closure breaks under
/// the flag; neither is true here — nothing `tutti-types` depends on reacts to
/// `cfg(loom)`.)
mod sync {
    #[cfg(loom)]
    pub(super) use loom::{
        hint::spin_loop,
        sync::atomic::{fence, AtomicPtr, AtomicUsize, Ordering},
        sync::Mutex,
    };
    #[cfg(not(loom))]
    pub(super) use std::{
        hint::spin_loop,
        sync::atomic::{fence, AtomicPtr, AtomicUsize, Ordering},
        sync::Mutex,
    };
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

/// Slot value: no reader.
const FREE: usize = 0;
/// Slot value: a reader has claimed the slot but not yet announced what it
/// read. Neither this nor [`FREE`] can collide with an announced address: an
/// `Arc`'s data pointer always points into a real heap block, past the two
/// `usize` counts at its start (even for a zero-sized `T`), so it is never 0
/// or 1.
const CLAIMED: usize = 1;

/// [`RtRef::slot`] for a reader on the overflow counter rather than a slot.
const OVERFLOW: usize = usize::MAX;

/// The slots, on their own cache line: readers write them, and keeping them
/// off the line holding `current` means a read does not invalidate the pointer
/// every other reader is about to load.
#[repr(align(64))]
struct Slots([AtomicUsize; READER_SLOTS]);

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
/// it clears a reader slot; it does not decrement an `Arc` the way an owning
/// handle would, and there is no code on that path that could run a
/// destructor. That matters because the audio thread is the one place a
/// deallocation must not happen: if the callback held the last reference to a
/// retired value, it would run `free` on its `Vec`s inside the block.
///
/// Retired values are freed by [`publish`](Self::publish), on the publishing
/// thread — or, when a reader still holds one at that moment, by a later
/// publish or by the cell's own drop. The cost is real, but it always lands
/// where blocking is allowed.
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
/// or many threads reading together) is safe: the extra readers take an
/// overflow counter instead, which is just as wait-free and allocation-free.
/// What it costs is reclamation, not the reader: while any overflow reader is
/// live, a publish frees nothing and leaves every retired value for the next
/// publish. Prefer one read per block, passed down.
pub struct RtPublish<T> {
    /// The current value, from [`Arc::into_raw`]. The cell owns that one strong
    /// count; [`Drop`] gives it back.
    current: AtomicPtr<T>,
    /// Live readers that found no free slot. Non-zero means "some reader holds
    /// *some* value, unknown which", so nothing retired may be freed.
    overflow: AtomicUsize,
    /// Per-reader hazard slots: `FREE`, `CLAIMED`, or the address of the value
    /// the reader holds.
    slots: Slots,
    /// Values swapped out but possibly still held by a reader. Touched only by
    /// `publish` and `Drop`, never by a reader — that is the whole design.
    /// The mutex also serializes concurrent publishers.
    retired: Mutex<Vec<Arc<T>>>,
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
        Self {
            current: AtomicPtr::new(Arc::into_raw(value).cast_mut()),
            overflow: AtomicUsize::new(0),
            slots: Slots(std::array::from_fn(|_| AtomicUsize::new(FREE))),
            // No capacity reserved: the list is only ever touched on the
            // control side, where growing it is allowed.
            retired: Mutex::new(Vec::new()),
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
        // Step 1: register. One CAS per slot tried, never a retry of the same
        // slot, so this is bounded by READER_SLOTS whatever else is running.
        // `Relaxed` is enough: the fence below orders this store against the
        // publisher's slot scan, and the slot carries no data to acquire.
        let slot = self
            .slots
            .0
            .iter()
            .position(|s| {
                s.compare_exchange(FREE, CLAIMED, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            })
            .unwrap_or_else(|| {
                self.overflow.fetch_add(1, Ordering::Relaxed);
                OVERFLOW
            });

        // Step 2: the reader half of the Dekker pair (see the module docs).
        // Without it, the load below may be satisfied before the claim above
        // is visible, and a publisher can scan an apparently empty slot,
        // free the value, and hand this reader a dangling pointer.
        fence(Ordering::SeqCst);

        // Step 3: `Acquire` pairs with the publisher's `AcqRel` swap, so the
        // pointee is fully constructed from this thread's point of view.
        let ptr = self.current.load(Ordering::Acquire);

        // Step 4: announce. `Relaxed`: the publisher only compares the address.
        // What it needs ordered — our reads of the value before its free — is
        // carried by the `Release` in `RtRef::drop`.
        if slot != OVERFLOW {
            self.slots.0[slot].store(ptr.addr(), Ordering::Relaxed);
        }

        RtRef {
            cell: self,
            // `current` is only ever set from `Arc::into_raw`, which is never
            // null.
            value: NonNull::new(ptr).expect("RtPublish::current is never null"),
            slot,
            _not_send: PhantomData,
        }
    }

    /// Publish a new value. **Control-thread only.**
    ///
    /// Swaps the value in, then frees every retired value no reader still
    /// holds — including the outgoing one, if nobody is reading it. It may
    /// spin briefly on a reader caught between claiming a slot and announcing
    /// what it read (a handful of instructions on the reader's side), and it
    /// takes a mutex that serializes publishers. Both costs — the wait and the
    /// free — belong to the caller, which is the entire point: neither reaches
    /// the callback.
    ///
    /// A retired value that a reader *is* holding is not waited for; it stays
    /// retired and is freed by the next publish after the reader lets go, or
    /// when the cell drops.
    ///
    /// Calling this from the audio thread would stall the callback *and* free
    /// inside it, defeating the type.
    pub fn publish(&self, value: Arc<T>) {
        let freeable = {
            // A poisoned lock means a publisher panicked, which can only have
            // happened in `Vec::push`'s allocation — the list is still a valid
            // list of owned `Arc`s, so carrying on is correct.
            let mut retired = self
                .retired
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);

            let new = Arc::into_raw(value).cast_mut();
            // `Release` publishes the new value's contents to readers'
            // `Acquire` load; `Acquire` makes the outgoing value's contents
            // ours before we drop it.
            let old = self.current.swap(new, Ordering::AcqRel);
            // SAFETY: `old` came out of `current`, which only ever holds a
            // pointer from `Arc::into_raw` whose strong count the cell owns.
            // The swap removed it from `current`, so this is the one place
            // that count is reclaimed — no other path can see `old` in
            // `current` again.
            retired.push(unsafe { Arc::from_raw(old) });

            self.take_unprotected(&mut retired)
        };
        // Freed outside the lock: `T::drop` is arbitrary code, and one that
        // published to this cell (or panicked) must not do it under our mutex.
        drop(freeable);
    }

    /// The publisher half of the protocol: remove from `retired` every value
    /// no reader can still be holding, and hand them back to be dropped.
    fn take_unprotected(&self, retired: &mut Vec<Arc<T>>) -> Vec<Arc<T>> {
        // The publisher half of the Dekker pair: orders the swap in `publish`
        // before the slot and counter reads below.
        fence(Ordering::SeqCst);

        // `Acquire` pairs with the `Release` decrement in `RtRef::drop`, so a
        // finished overflow reader's reads happen-before our free.
        if self.overflow.load(Ordering::Acquire) != 0 {
            return Vec::new();
        }

        let mut held = [FREE; READER_SLOTS];
        for (held, slot) in held.iter_mut().zip(&self.slots.0) {
            // `Acquire`, pairing with `RtRef::drop`'s `Release`: a slot seen
            // `FREE` after a read means that reader's accesses are done.
            let mut seen = slot.load(Ordering::Acquire);
            // A reader between claim and announce. It is running a fence and
            // a load, not waiting on anything, so this terminates as soon as
            // it is scheduled — and the spinning is on the control thread.
            while seen == CLAIMED {
                spin_loop();
                seen = slot.load(Ordering::Acquire);
            }
            *held = seen;
        }

        let (keep, free): (Vec<_>, Vec<_>) = retired
            .drain(..)
            .partition(|arc| held.contains(&Arc::as_ptr(arc).addr()));
        *retired = keep;
        free
    }

    /// How many readers currently hold an [`RtRef`] on the overflow path.
    #[cfg(all(test, not(loom)))]
    fn overflow_readers(&self) -> usize {
        self.overflow.load(Ordering::SeqCst)
    }

    /// How many swapped-out values are still waiting to be freed.
    #[cfg(all(test, not(loom)))]
    fn retired_len(&self) -> usize {
        self.retired
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

impl<T> Drop for RtPublish<T> {
    fn drop(&mut self) {
        // `&mut self` means no `RtRef` is alive (each borrows the cell), so
        // nothing is protected and everything can go.
        let current = self.current.load(Ordering::Acquire);
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

/// A borrow of the value inside an [`RtPublish`], valid for as long as it is
/// held.
///
/// Deliberately not `Send`, and tied to the cell's lifetime. Between them, a
/// guard cannot be stashed in a struct field, moved to another thread, or
/// outlive the cell — so "held across blocks", the failure mode a review rule
/// would otherwise have to catch, is a compile error instead.
///
/// There is no way to get an owning `Arc` back out. That is not an oversight.
/// Dropping one clears a reader slot and does nothing else: it cannot free.
pub struct RtRef<'a, T> {
    cell: &'a RtPublish<T>,
    /// The value this reader announced. Kept alive by that announcement, not
    /// by a refcount.
    value: NonNull<T>,
    /// Index into `cell.slots`, or [`OVERFLOW`].
    slot: usize,
    /// `*const ()` is neither `Send` nor `Sync`, which is what pins the guard
    /// to the thread that took it. (`NonNull` alone would do the same; this
    /// says so on purpose rather than by accident of representation.)
    _not_send: PhantomData<*const ()>,
}

impl<T> Deref for RtRef<'_, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: `value` was read from `current` after this reader registered
        // (slot claim or overflow increment, then the `SeqCst` fence), and the
        // registration stays in place until `drop`. A publisher frees a
        // retired value only after its own fence finds no slot announcing it
        // and a zero overflow count; the fence pair guarantees it cannot miss
        // this registration while also having retired a value this reader
        // loaded. The `Acquire` load makes the pointee's construction visible.
        // The borrow is tied to `&self`, which cannot outlive the registration.
        unsafe { self.value.as_ref() }
    }
}

impl<T> Drop for RtRef<'_, T> {
    #[inline]
    fn drop(&mut self) {
        // `Release`: every read this thread made through the ref happens-before
        // a publisher's `Acquire` of the cleared slot or decremented counter,
        // and therefore before the free. Nothing here can run a destructor.
        if self.slot == OVERFLOW {
            self.cell.overflow.fetch_sub(1, Ordering::Release);
        } else {
            self.cell.slots.0[self.slot].store(FREE, Ordering::Release);
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
    /// Mutation: in `take_unprotected`, delete the overflow-count early
    /// return. The second publish then frees value 1 while the overflow
    /// readers still hold it, and `sites` is non-empty at the first check.
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
            "overflow readers hold an unknown value, so nothing may be freed"
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
    /// then reports a use-after-free (the plain run rarely does on x86, whose
    /// CAS is already a full barrier; this is why the model check exists).
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
