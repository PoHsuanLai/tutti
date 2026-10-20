//! Like `arc-swap`, but readers only ever borrow: the writer keeps ownership,
//! so the read side never frees.
//!
//! [`ArcLend<T>`] holds an `Arc<T>` that one side replaces with
//! [`publish`](ArcLend::publish) and any number of threads read with
//! [`lend`](ArcLend::lend). A lend returns a [`Loan`]: a `!Send` borrow tied to
//! the cell, not an owning handle. Dropping a loan clears a slot or bumps a
//! counter, and nothing else. It cannot run a destructor, because the reader
//! never owns anything a destructor could be run on.
//!
//! # The problem
//!
//! Some threads have a deadline and must not block, allocate or free on it: an
//! audio callback with a millisecond to fill a buffer, a game's fixed-rate
//! simulation tick, a control loop, a packet path. They still need state that
//! is too large for an atomic, such as a routing table, a level's collision
//! data, a filter's coefficients or a sampling configuration, and that state
//! changes while they run.
//!
//! Every publish of a new value retires an old one, and the old one has to be
//! freed *somewhere*. With an `Arc`, it is freed by whoever drops the last
//! strong count. If that can be the deadline thread, then sooner or later it
//! will be: a `free` of a large `Vec` (or a destructor that does I/O) lands
//! inside the deadline, at a time no test chose.
//!
//! # The insight
//!
//! Do not let the reader own anything. If a reader only *announces* what it is
//! looking at and later *un-announces* it, then the last owner of every value
//! is always the cell, and the cell is only ever reached for reclamation by the
//! publisher.
//!
//! - **Hazard slots.** A reader claims one of a few slots, announces the
//!   pointer it loaded, and clears the slot when its loan drops. A publisher
//!   frees a retired value only if no slot announces it. A `SeqCst` fence on
//!   each side orders the claim against the publisher's scan.
//! - **Overflow epochs.** When every slot is taken, a reader registers in the
//!   current *epoch* with one atomic add instead. An epoch pins the short run
//!   of values that were current while it was open. The lend still never
//!   fails, waits or allocates.
//! - **A retirement list only the publisher touches.** Swapped-out values wait
//!   there, behind a mutex no reader ever takes, until a publish finds them
//!   unannounced and unpinned. The free runs on the publisher's thread,
//!   outside the lock.
//!
//! The reader announces and un-announces; it never owns. That is the whole
//! design. The protocol and its memory-ordering argument are in
//! [`docs::internal`].
//!
//! # Example
//!
//! ```
//! use std::sync::Arc;
//! use arc_lend::ArcLend;
//!
//! #[derive(Debug)]
//! struct Mix { gains: Vec<f32> }
//!
//! let cell = Arc::new(ArcLend::new(Mix { gains: vec![1.0; 4] }));
//!
//! // The deadline thread: one lend per iteration, dropped before the next.
//! let reader = {
//!     let cell = Arc::clone(&cell);
//!     std::thread::spawn(move || {
//!         for _ in 0..1000 {
//!             let mix = cell.lend();
//!             let total: f32 = mix.gains.iter().sum();
//!             assert!(total > 0.0);
//!         } // the loan drops here: a store, never a free
//!     })
//! };
//!
//! // The publisher: build off the deadline thread, publish, and absorb the
//! // free of whatever no reader still holds.
//! for n in 1..100 {
//!     cell.publish(Arc::new(Mix { gains: vec![0.5; 4 + n % 3] }));
//! }
//! reader.join().unwrap();
//! assert!(cell.retired_len() <= 1);
//! ```
//!
//! # How it compares
//!
//! Each row is the crate's documented or source-visible behaviour, checked
//! against the version named; the sources are cited under [Sources](#sources).
//! "Waits" means the operation can loop on another thread's progress.
//!
//! | | Reader holds | Reader frees? | Reader allocates? | Reader waits? | Writer waits on readers? | Readers |
//! |---|---|---|---|---|---|---|
//! | **arc-lend** | a borrow | never | never | no (wait-free) | only on a reader mid-lend, never on a held loan | any |
//! | `arc-swap` 1.9.3 `load` | a `Guard`, which a store can promote to an owned `Arc` | yes, if its guard was promoted and is the last owner | on a thread's first load, if no free debt node exists (`debt/list.rs:169`) | lock-free; wait-free almost always | no | any |
//! | `basedrop` 0.1.3 `SharedCell` | an owned `Shared<T>` | no: the last drop queues it for a `Collector` | no | no | yes: `replace` spins until no reader is mid-`get` | any |
//! | Firewheel 0.14 `ArcGc` | an owned `ArcGc<T>` | no: a registry keeps a count until `collect` | no | no | n/a: a pointer type, swapped by other means; registering takes a global mutex | any |
//! | `left-right` 0.11.8 | a borrow (`ReadGuard`) | no | no (a `ReadHandle` allocates once, when made) | no | yes: `publish` waits for every reader to leave the old copy | one handle per reader |
//! | `triple_buffer` 9.0.0 | a borrow of its own buffer | no | no | no | no | one |
//! | `rtrb` 0.4.0 + a return ring | an owned value it popped | only if it does not send it back | no | no | no | one |
//! | farbot `NonRealtimeMutatable` | a borrow | no | no | no | yes: the writer's CAS loops until the reader releases | one |
//! | `Mutex` / `RwLock` | a lock guard | it can, if it drops the last `Arc` it cloned | no | yes: it blocks | yes | any |
//!
//! ## When to prefer something else
//!
//! - **`arc-swap`** is the better general-purpose cell. Its uncontended loads
//!   cost about what a lend does with one reader, stay flat as readers are
//!   added where a lend does not, and nest past 8 more cheaply (see
//!   [the benchmark](https://github.com/PoHsuanLai/Tutti/blob/main/crates/arc-lend/BENCHMARKS.md)). It has a far larger API
//!   (`rcu`, `compare_and_swap`, `Cache`, `Access` projections) and years of
//!   use. If the reader may free, or a
//!   thread's first load may allocate, it is the right choice. It can also be
//!   made safe for a deadline by discipline: prewarm each reader thread, and
//!   have the writer keep retired `Arc`s until their count is one. arc-lend
//!   makes that a property of the type instead of a protocol for its users.
//! - **`triple_buffer`** is better for one reader and a `Clone` value that is
//!   cheap to overwrite in place: no `Arc` per publish, and its reader is a
//!   single swap.
//! - **`left-right`** is better when the writer applies small operations to a
//!   large structure (an oplog against two copies) and can afford to wait for
//!   readers, or use `try_publish`.
//! - **`basedrop`** and **Firewheel's `ArcGc`** are better when the deadline
//!   thread must *own* values (store them, pass them on, drop them whenever),
//!   and a collector thread is acceptable. arc-lend's readers cannot keep
//!   anything past their loan.
//! - **`rtrb`** with a return ring suits one reader that takes ownership of a
//!   stream of values and sends each back to be freed.
//! - **A seqlock** (crill's `seqlock_object`, P3825) suits a small
//!   trivially-copyable value written often. Its readers copy, and may retry
//!   against a concurrent write.
//! - **A plain atomic** (or `Mutex`, off the deadline thread) for anything that
//!   fits in one, or is never read on a deadline.
//!
//! # Cost
//!
//! Counts of operations, not measured times. The measured comparison with
//! `arc-swap`, `basedrop`, `left-right` and the locks is in
//! [BENCHMARKS.md](https://github.com/PoHsuanLai/Tutti/blob/main/crates/arc-lend/BENCHMARKS.md), one run on a shared VM. In it no arc-lend
//! reader allocated or freed, and one reader's lend cost what an `arc-swap`
//! load did (47 ns against 51 ns p50). arc-lend lost where readers share the
//! cell: its slots are one cache line for all readers, so the reader p50 rose
//! to 300 ns at four readers (`arc-swap` stayed at 51 ns), nested lends past
//! the slots were slower than `arc-swap`'s, and the publisher's tail grew when
//! a reader was preempted mid-lend. A staircase of parked loans pinned every
//! retired value, as [`docs::limitations`] describes.
//!
//! | Operation | Thread | What it does |
//! |---|---|---|
//! | [`lend`](ArcLend::lend), slot path | reader | ≤ `SLOTS` relaxed CASes, one `SeqCst` fence, one `Acquire` load, one relaxed store |
//! | [`lend`](ArcLend::lend), overflow path | reader | one `Acquire` `fetch_add`, one `Acquire` load, one `Release` `fetch_add` |
//! | drop a [`Loan`] | reader | one `Release` store (slot) or `fetch_add` (overflow) |
//! | [`publish`](ArcLend::publish) | publisher | a mutex; one allocation for the free list (and amortized growth of the retired list); one swap and one `SeqCst` fence; an epoch advance that may spin on readers between registering and loading; a scan of `SLOTS` slots that may spin on readers between claiming and announcing; a filter over the retired list; the frees, after the lock |
//! | [`retired_len`](ArcLend::retired_len), [`epoch_stalls`](ArcLend::epoch_stalls) | publisher | the mutex |
//!
//! No reader operation allocates, frees, takes a lock or retries. Memory is
//! `SLOTS` words on their own cache line, two words per epoch, the pointer,
//! the epoch word, and the retired list.
//!
//! # Parameters
//!
//! `ArcLend<T, SLOTS = 8, EPOCHS = 4>`. `SLOTS` is how many loans of one cell
//! can be live before lends take the coarser overflow path; `EPOCHS` is how
//! many overflow epochs the cell cycles through. Both are checked at compile
//! time (`1 ≤ SLOTS ≤ 1024`, `2 ≤ EPOCHS ≤ 64`). The defaults suit a few reader
//! threads with one loan each.
//!
//! # Guides
//!
//! - [`docs::patterns`]: how to use it well.
//! - [`docs::limitations`]: what it does not do, and where memory can grow.
//! - [`docs::internal`]: the protocol and why it is sound.
//!
//! # Verification
//!
//! The protocol is checked against the shipped code, not a replica: under
//! `--cfg loom` its atomics, fences and mutex are loom's, and `tests/loom.rs`
//! models readers against publishers (its mutation record lists each ordering
//! it catches, and the one it cannot). Miri runs the unit tests, including a
//! multi-reader stress test, over many scheduling seeds:
//!
//! ```text
//! LOOM_MAX_PREEMPTIONS=4 RUSTFLAGS="--cfg loom" cargo test -p arc-lend --release --test loom
//! MIRIFLAGS="-Zmiri-many-seeds=0..16" cargo +nightly miri test -p arc-lend --lib
//! ```
//!
//! # Sources
//!
//! The comparison was checked against these sources (line numbers at the
//! version named):
//!
//! - `arc-swap` 1.9.3. A store pays every outstanding debt with an owned
//!   count (`src/debt/mod.rs:81`, `pay_all`, called from
//!   `src/strategy/hybrid.rs:205`); a promoted guard's drop then releases its
//!   `Arc` on the reader (`src/strategy/hybrid.rs:105-125`, the
//!   `ManuallyDrop::drop` at line 124). Its docs say the same
//!   (`src/docs/performance.rs:55-60`). A thread without a debt node gets
//!   one on its first load (`src/debt/list.rs:223-228`), allocating it if
//!   none is free (`src/debt/list.rs:169`, `Box::leak(Box::<Node>::default())`).
//!   Readers are lock-free and "most of the time" wait-free
//!   (`src/docs/performance.rs:7-9`).
//! - `basedrop` 0.1.3. `SharedCell::get` is a reader-count increment, a
//!   clone and a decrement (`src/shared_cell.rs:61-74`); `replace` spins
//!   until the count is zero (`src/shared_cell.rs:117`). `Shared::drop`
//!   queues the last count to the collector rather than freeing
//!   (`src/shared.rs:101-111`, `src/collector.rs:97-102`).
//! - Firewheel 0.14 (`firewheel-core`, commit `34b35cad`). `ArcGc::new`
//!   registers a strong count in a global `Mutex<Vec<_>>`
//!   (`src/collector.rs:59`, `:75`, `:239`); `collect` drops entries whose
//!   count is 1 (`src/collector.rs:93`); `ArcGc::drop` only flags
//!   (`src/collector.rs:312-316`, `:79-85`).
//! - `left-right` 0.11.8. `publish` waits in a spin/yield loop until every
//!   reader has moved past the swap (`src/write.rs:249-300`); `try_publish`
//!   returns instead (`src/write.rs:320-330`). Making a `ReadHandle`
//!   allocates and locks the epoch list (`src/read.rs:87-91`); `enter` is
//!   an increment (`src/read.rs:172`).
//! - `triple_buffer` 9.0.0. Single-producer single-consumer
//!   (`src/lib.rs:125`); `Output::read` takes `&mut self` (`src/lib.rs:432`).
//! - `rtrb` 0.4.0. A single-producer single-consumer ring (`src/lib.rs:1`).
//! - farbot (commit `d8f132c2`). The realtime side takes the pointer with an
//!   `exchange(nullptr)` (`include/farbot/detail/RealtimeObject.tcc:44`), so
//!   there is one realtime reader, and the writer's `compare_exchange_weak`
//!   loops until it is given back (`RealtimeObject.tcc:69-72`).
//! - crill (commit `895eaf2f`). `seqlock_object`'s readers "can block if
//!   there is a concurrent write" (`include/crill/seqlock_object.h:21-23`).

mod lend;

pub mod docs;

pub use lend::{ArcLend, Loan, DEFAULT_EPOCHS, DEFAULT_SLOTS, MAX_EPOCHS, MAX_SLOTS};

/// The README's examples, compiled and run as doctests so it cannot drift
/// from the API.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
