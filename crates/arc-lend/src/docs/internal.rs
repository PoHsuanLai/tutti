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
//! # The state
//!
//! - **`current`**: an `AtomicPtr` to the current value (an `Arc` turned into
//!   a raw pointer, so the cell owns one strong count), on a cache line of its
//!   own.
//! - **The slot list**: one word per registered [`Reader`](crate::Reader),
//!   each on a cache line of its own, in fixed-size blocks. The list only
//!   grows, a block at a time, and a block never moves, so a reader keeps a
//!   pointer to its slot. A slot's word is one of:
//!   - `FREE` (0): no loan;
//!   - a **claim**: `(generation << 1) | 1`, odd. The reader is between
//!     starting a lend and saying what it loaded;
//!   - an **announcement**: the address of the value the reader holds, even
//!     and nonzero (an `Arc`'s data pointer is both).
//!
//!   Beside the word, an `owned` flag says whether a live reader owns the
//!   slot.
//! - **The control block**, behind a mutex no reader ever takes: the retired
//!   list (each value with the sequence number it had while current), the
//!   current sequence number `seq`, `scanned_at` (what `seq` was at the last
//!   scan), and per slot the claim the last scan saw there with its *floor*.
//!
//! # Registering a reader
//!
//! [`ArcLend::reader`](crate::ArcLend::reader), under the control lock: find
//! a slot whose `owned` is false (`Acquire`, pairing with the previous owner's
//! `Release` when it dropped), starting where the last registration stopped;
//! if every slot is owned, append a block. Set `owned`, reset the word to
//! `FREE` (a previous owner may have `mem::forget`ed a loan), and hand the
//! slot to the new reader. Dropping a reader is one `Release` store of
//! `owned = false`. The list keeps its high-water mark.
//!
//! # A lend
//!
//! [`Reader::lend`](crate::Reader::lend), on the reader's own slot:
//!
//! 1. **claim**: store the reader's next claim word (`Release`);
//! 2. `fence(SeqCst)`;
//! 3. load `current` (`Acquire`, pairing with the publisher's swap, so the
//!    value is seen fully built);
//! 4. **announce**: store the loaded address (`Relaxed`).
//!
//! Dropping the [`Loan`](crate::Loan) stores `FREE` (`Release`).
//!
//! That is the whole reader side: four stores to a line no other reader
//! touches, one fence and one load. It never touches a refcount, takes a lock,
//! loops, retries or frees. The lend is **wait-free** and
//! **allocation-free**, and so is dropping a reader.
//!
//! # A publish, under the control lock
//!
//! 1. reserve room in the retired list and take the reused free buffer (the
//!    only allocations, before anything changes, so a panic there leaves the
//!    cell untouched);
//! 2. swap the new pointer in (`AcqRel`), bump `seq`, and retire the old value
//!    with its sequence number;
//! 3. `fence(SeqCst)`;
//! 4. **scan**: read every slot once (`Acquire`):
//!    - `FREE`: pins nothing;
//!    - an announcement: pins the value at that address;
//!    - a claim: pins every retired value with a sequence number at or above
//!      the claim's **floor**. If the last scan saw *this same claim word*,
//!      the floor is the one recorded then: the reader has been mid-lend since
//!      before that scan. Otherwise the floor is `scanned_at`, the sequence
//!      number current at the last scan. Record the claim and its floor for
//!      the next scan;
//!
//!    then set `scanned_at = seq`, and move every retired value that is below
//!    the lowest floor and not announced to the free buffer: one
//!    order-preserving pass over the list, with announced addresses sorted
//!    and looked up by binary search.
//!
//! After the lock is released, the publish drops the freeable values and
//! hands the emptied buffer back for the next publish.
//! [`reclaim`](crate::ArcLend::reclaim) is steps 3 and 4 alone.
//!
//! **The publisher never waits on a reader.** A claim is not spun on: it pins
//! what the reader might load and the next scan looks again. A held loan pins
//! only its own value. A publish that waited on a held loan would deadlock a
//! thread that lends and then publishes; one that waited on a claim (as this
//! crate's earlier protocol did) would be stalled by a reader descheduled
//! between two of its instructions.
//!
//! # Why it is sound
//!
//! The argument is in C++20's terms (Rust's atomics model), and each step names
//! the rule it rests on. Take a reader whose lend loads value `V`, and a scan
//! `S` (by a publish or a reclaim) that frees `V`. `V` was retired by a swap
//! `X` that happens before `S`'s fence `Fp` (the same publish, or an earlier
//! one under the mutex). Call the reader's claim `C`, its fence `Fr`, its load
//! `R`. `SeqCst` fences are totally ordered.
//!
//! **If `Fp` precedes `Fr`**, then `X` is sequenced before `Fp` and `R` after
//! `Fr`, so `R` reads `X` or a later write of `current` ([atomics.order]/4,
//! the fence-to-fence clause). It cannot load `V`.
//!
//! **If `Fr` precedes `Fp`**, `S`'s read of the slot sees `C` or a later write
//! of it (the same clause, with `C` sequenced before `Fr`). Each case:
//!
//! - **The announcement of `V`**: `V` is pinned. Not freed.
//! - **A later write by the same reader**: `FREE` from the loan's drop, or the
//!   next lend's claim, both `Release`; or a later announcement, which the
//!   next lend's fence (`SeqCst`, so also a release fence) precedes. The
//!   scan's `Acquire` read synchronizes with it, and everything the reader
//!   did through its loan of `V` happens before the scan, and so before the
//!   free. (This is why the claim is `Release`: the `claim_relaxed` mutation
//!   in the loom model frees `V` under the previous loan.)
//! - **A write by a later owner of the slot**: the registration that handed
//!   the slot on read the reader's `Release` drop of `owned` with `Acquire`,
//!   and runs under the mutex before `S`; the new owner's stores happen after
//!   that registration. Either way, the reader's loan happens before the
//!   free.
//! - **`C` itself**: the reader is mid-lend, and `S` keeps every retired value
//!   with a sequence number at or above the claim's floor. It remains to show
//!   `V`'s is. Let `S'` be the scan before `S`, with fence `Fp'`.
//!   - *`S'` saw a different word in this slot, or the slot did not exist
//!     yet.* The floor is `scanned_at`, the sequence number of the value
//!     current at `S'`. `S'` happens before `S` (the mutex), and `S` read
//!     `C`, so by read-read coherence `S'` read `C` or an earlier write; not
//!     `C`, so an earlier one. Had `Fr` preceded `Fp'`, `S'` would have read
//!     `C` or later, so `Fp'` precedes `Fr`. (If the slot was registered after
//!     `S'`, then `S'` happens before the registration, which happens before
//!     `C`, which gives the same order.) Every swap up to `S'` happens before
//!     `Fp'`, so `R` reads the value current at `S'` or a later one: its
//!     sequence number is at least `scanned_at`.
//!   - *`S'` saw this same claim word.* The reader has not moved since, and
//!     the floor is the one `S'` recorded, which by the same argument (one
//!     scan further back) bounds what `R` can load.
//!   - *A reused word*: a claim with the same generation after the counter
//!     wrapped, or a word recorded before the slot was handed on. The scan
//!     keeps the older floor; `scanned_at` only grows, so an older floor pins
//!     a superset. Mistaking one claim for another only keeps more.
//!
//! A slot `S` reads as `FREE` before `C` is not a case: `Fr` preceding `Fp`
//! puts `S`'s read at `C` or later in the slot's modification order.
//!
//! loom checks this against the shipped code (see the model's docs, and its
//! mutation record: each ordering above, removed, fails a model). Its
//! `SeqCst` fences are *stronger* than C++'s (it gives them a total order that
//! also constrains surrounding accesses), so a passing model supports the
//! argument above rather than replacing it; miri's weak-memory emulation over
//! the stress test is the second, independent check.
//!
//! # What a claim can pin
//!
//! An announcement pins one value, so parked loans pin at most one value per
//! registered reader. A claim pins every value retired from its floor on: for
//! a reader that announces promptly, the one or two values current around
//! its lend. A reader preempted between its claim and its announcement (two
//! instructions apart) pins everything retired while it is preempted, and the
//! first scan after it announces frees all of it but the one it holds. Where
//! that leaves the retired list is in [`limitations`](super::limitations).
