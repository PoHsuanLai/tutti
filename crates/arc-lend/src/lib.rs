//! Like `arc-swap`, but readers only ever borrow: the writer keeps ownership,
//! so the read side never frees.
//!
//! [`ArcLend<T>`] holds an `Arc<T>` that one side replaces with
//! [`publish`](ArcLend::publish). Each thread that reads it registers a
//! [`Reader`] once, with [`reader`](ArcLend::reader), and then lends through
//! it with [`Reader::lend`]. A lend returns a [`Loan`]: a `!Send` borrow, not
//! an owning handle. Dropping a loan clears the reader's slot, and nothing
//! else. It cannot run a destructor, because the reader never owns anything
//! a destructor could be run on.
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
//! - **A slot per reader.** Registering a [`Reader`] gives it one hazard
//!   slot, on a cache line of its own. A lend writes a claim there, loads the
//!   pointer, and announces it; the loan's drop clears it. No reader writes
//!   another's line, and no reader loops. A `SeqCst` fence on each side
//!   orders the claim against the publisher's scan.
//! - **A publisher that never waits.** A publish frees every retired value
//!   no slot announces. A reader caught between its claim and its
//!   announcement is not waited for: its claim carries a generation, and the
//!   publisher keeps every value that reader could still load until a later
//!   scan sees it has moved on.
//! - **A retirement list only the publisher touches.** Swapped-out values wait
//!   there, behind a mutex no reader ever takes. The free runs on the
//!   publisher's thread, outside the lock.
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
//! // Register on the control side (this may allocate), then move the
//! // reader to the deadline thread: one lend per iteration.
//! let mut reader = cell.reader();
//! let deadline = std::thread::spawn(move || {
//!     for _ in 0..1000 {
//!         let mix = reader.lend();
//!         let total: f32 = mix.gains.iter().sum();
//!         assert!(total > 0.0);
//!     } // the loan drops here: a store, never a free
//! });
//!
//! // The publisher: build off the deadline thread, publish, and absorb the
//! // free of whatever no reader still holds.
//! for n in 1..100 {
//!     cell.publish(Arc::new(Mix { gains: vec![0.5; 4 + n % 3] }));
//! }
//! deadline.join().unwrap();
//! assert_eq!(cell.reclaim(), 0); // the reader is gone: nothing is pinned
//! ```
//!
//! # How it compares
//!
//! Each row is the crate's documented or source-visible behaviour, checked
//! against the version named; the sources are cited under [Sources](#sources).
//! "Waits" means the operation can loop on another thread's progress.
//!
//! | | Reader holds | Reader frees? | Reader allocates? | Reader waits? | Writer waits on readers? | A reader preempted mid-read pins | Readers |
//! |---|---|---|---|---|---|---|---|
//! | **arc-lend** | a borrow | never | never (registering may, on the control side) | no (wait-free) | never | what is retired while it is preempted (2902 values at peak in the benchmark, under a flat-out publisher) | one registered `Reader` each |
//! | `arc-swap` 1.9.3 `load` | a `Guard`, which a store can promote to an owned `Arc` | yes, if its guard was promoted and is the last owner | on a thread's first load, if no free debt node exists (`debt/list.rs:169`) | lock-free; wait-free almost always | no | the value it loaded | any |
//! | `basedrop` 0.1.3 `SharedCell` | an owned `Shared<T>` | no: the last drop queues it for a `Collector` | no | no | yes: `replace` spins until no reader is mid-`get` | nothing (the writer spins instead) | any |
//! | Firewheel 0.14 `ArcGc` | an owned `ArcGc<T>` | no: a registry keeps a count until `collect` | no | no | n/a: a pointer type, swapped by other means; registering takes a global mutex | the value it holds | any |
//! | `left-right` 0.11.8 | a borrow (`ReadGuard`) | no | no (a `ReadHandle` allocates once, when made) | no | yes: `publish` waits for every reader to leave the old copy | nothing (the writer waits instead) | one handle per reader |
//! | `triple_buffer` 9.0.0 | a borrow of its own buffer | no | no | no | no | its own buffer | one |
//! | `rtrb` 0.4.0 + a return ring | an owned value it popped | only if it does not send it back | no | no | no | the value it popped | one |
//! | farbot `NonRealtimeMutatable` | a borrow | no | no | no | yes: the writer's CAS loops until the reader releases | nothing (the writer waits instead) | one |
//! | `Mutex` / `RwLock` | a lock guard | it can, if it drops the last `Arc` it cloned | no | yes: it blocks | yes | nothing (the writer blocks instead) | any |
//!
//! ## When to prefer something else
//!
//! - **`arc-swap`** is the better general-purpose cell. Its loads cost about
//!   what a lend does (see
//!   [the benchmark](https://github.com/PoHsuanLai/Tutti/blob/main/crates/arc-lend/BENCHMARKS.md)),
//!   need no registration, and its writer's cost does not grow with the
//!   readers. A reader preempted mid-load pins only the value it loaded,
//!   where arc-lend's pins everything retired meanwhile: under a fast
//!   publisher and frequent preemption, arc-swap holds less memory. It has a far larger API (`rcu`, `compare_and_swap`, `Cache`,
//!   `Access` projections) and years of use. If the reader may free, or a
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
//! A lend is a `Release` store, a `SeqCst` fence, an `Acquire` load and a
//! `Relaxed` store, all on the reader's own cache line, plus a `Release`
//! store when the loan drops. A publish takes a mutex, swaps, fences, reads
//! every registered slot once and filters the retired list once. The
//! operation counts, and what they measured against `arc-swap`, `basedrop`,
//! `left-right` and the locks, are in [`docs::performance`].
//!
//! In one run on a shared VM, no arc-lend reader allocated or freed; a lend
//! cost 41-42 ns p50 per block at one to four readers (`arc-swap`: 51 ns);
//! nested lends cost about one lend per level; and the publisher never
//! waited on a reader. It lost where a publish scans many registered readers
//! (1343 ns at 1000), and where a reader preempted mid-lend under a flat-out
//! publisher pinned thousands of retired values until it ran again.
//!
//! # Guides
//!
//! - [`docs::patterns`]: how to use it well.
//! - [`docs::limitations`]: what it does not do, and where memory can grow.
//! - [`docs::performance`]: what it costs, measured.
//! - [`docs::internal`]: the protocol and why it is sound.
//!
//! # Verification
//!
//! The protocol is checked against the shipped code, not a replica: under
//! `--cfg loom` its atomics, fences and mutex are loom's, and `tests/loom.rs`
//! models readers against publishers (its mutation record lists each ordering
//! it catches). Miri runs the unit tests, including a multi-reader stress
//! test, over many scheduling seeds:
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

pub use lend::{ArcLend, Loan, Reader};

/// The README's examples, compiled and run as doctests so it cannot drift
/// from the API.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
