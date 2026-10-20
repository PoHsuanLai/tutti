//! The protocol, and why it is sound.
//!
//! This is the design document for [`ArcLend`](crate::ArcLend). The code in
//! `src/lend.rs` names the step each line implements; this page says what
//! the steps are and why they are enough.
//!
//! **A test that a reader never allocates cannot pin this property.** The
//! hazard is a race between a reader and a publisher, and a single-threaded
//! sampling test has no schedule that exhausts it. The guarantee lives in two
//! places that *can* be checked: the [`Loan`](crate::Loan) type (no owning
//! handle, checked at compile time), and the reclamation protocol below (no
//! code path on the reader side that frees, checked by the loom model in
//! `tests/loom.rs` and by miri over the unit tests).
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
//! ## The slot path: what a lend normally takes
//!
//! 1. claim a free slot with one CAS per slot tried (`FREE → CLAIMED`);
//! 2. `fence(SeqCst)`;
//! 3. load the current pointer (`Acquire`, pairing with the publisher's swap,
//!    so the value is seen fully built);
//! 4. announce that pointer's address in the slot.
//!
//! Dropping the [`Loan`](crate::Loan) stores `FREE` with `Release`.
//!
//! ## The overflow path: when every slot is taken
//!
//! A lend that finds no free slot registers in the **current overflow epoch**
//! instead, with one `fetch_add` on the word that names the epoch (so learning
//! the epoch and registering in it are one atomic step — there is no window in
//! which a reader has read a stale epoch but not yet registered). It then loads
//! the current pointer (`Acquire`) and bumps that epoch's `loaded` counter
//! (`Release`). Dropping the [`Loan`](crate::Loan) bumps the epoch's `exited` counter
//! (`Release`).
//!
//! An overflow reader never says which value it holds, but its epoch bounds
//! it: an epoch pins the contiguous run of sequence numbers that were current
//! from the moment it became current until the publisher *sealed* it. That is
//! what keeps a stuck overflow reader — more than the slot count of overlapping
//! readers, or a `Loan` passed to `mem::forget` — from stalling reclamation
//! forever: it pins its own epoch's run, a couple of values, and nothing else.
//!
//! A future version replaces this fallback with registered per-reader slots,
//! so that a reader has a single behaviour.
//!
//! ## What a reader does not do
//!
//! Either way, the reader never touches a refcount, never takes the lock, never
//! frees, and never loops: at most `SLOTS` CASes, or one `fetch_add`
//! plus one more. The lend is **wait-free** and **allocation-free**.
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
//!    (spinning on the publisher's thread; each is a few instructions from done,
//!    but the spin lasts as long as such a reader stays descheduled there,
//!    which is unbounded: `BENCHMARKS.md`, section 3).
//!    The sealed epoch pins `[its start, the new value]`;
//! 5. read every reader slot, spinning past one that is `CLAIMED` (a reader
//!    between claim and announce), and free — after dropping the lock — every
//!    retired value that no slot announces and no live epoch pins. The current
//!    epoch is always live and pins everything from its start on; a sealed
//!    epoch is live until its `exited` count reaches its registrations.
//!
//! A retired value a reader still holds is **not** waited for — a publish that
//! waited on a held loan would deadlock a thread that lends and then publishes.
//! It stays on the list and is freed by a later publish, or when the cell drops.
//! Either way the free runs on the publisher's side.
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
//! # The parameters
//!
//! `SLOTS` sizes the slot array, and so the bound on a lend's CASes. `EPOCHS`
//! sizes the epoch arrays, and the epoch word's index field is the fewest bits
//! that name every epoch: `ceil(log2(EPOCHS))`, which is 2 for the default 4,
//! exactly the packing the cell had when both were constants. The count takes
//! the remaining bits and is compared modulo their width (`COUNT_MASK`), which
//! is correct as long as fewer than `2^(usize::BITS - index bits)` overflow
//! readers are live in one epoch at once. `MAX_EPOCHS` (64) keeps that above
//! `2^26` on a 32-bit target. With one epoch the current epoch could never
//! advance, so `EPOCHS` starts at 2; with no slots every lend would overflow,
//! so `SLOTS` starts at 1. Both bounds are compile-time assertions.
//!
//! Where the retired list can still grow, and what the cell reports when it
//! does, is in [`limitations`](super::limitations).
