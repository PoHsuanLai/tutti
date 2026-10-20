//! What it costs: the operations, and what they measured.
//!
//! # Operation counts
//!
//! | Operation | Thread | What it does |
//! |---|---|---|
//! | [`ArcLend::reader`](crate::ArcLend::reader), `Reader::clone` | control | the mutex; a walk for an unowned slot from where the last registration stopped; one new block of slots (an allocation) when every slot is owned |
//! | [`Reader::lend`](crate::Reader::lend) | reader | a `Release` store, a `SeqCst` fence, an `Acquire` load, a `Relaxed` store, all on the reader's own cache line but the load |
//! | drop a [`Loan`](crate::Loan) | reader | one `Release` store |
//! | drop a [`Reader`](crate::Reader) | any | one `Release` store, and its `Arc` of the cell |
//! | [`publish`](crate::ArcLend::publish) | publisher | the mutex; one swap and one `SeqCst` fence; one `Acquire` load per slot up to the high-water mark; a sort of the announced addresses; one order-preserving pass over the retired list; the frees, after the lock. It allocates only when the retired list grows or the reused free buffer is too small |
//! | [`reclaim`](crate::ArcLend::reclaim) | publisher | a publish without the swap |
//! | [`retired_len`](crate::ArcLend::retired_len) | publisher | the mutex |
//!
//! No reader operation allocates, frees, takes a lock, loops or retries.
//! Memory is the pointer and one cache line per slot (128 bytes on x86-64 and
//! aarch64, 64 elsewhere), allocated 16 slots at a time and kept at the
//! high-water mark, plus the retired list and one claim record per slot on
//! the publisher's side.
//!
//! # Measured
//!
//! The comparison with `arc-swap`, `basedrop`, `left-right` and the locks is
//! in [BENCHMARKS.md](https://github.com/PoHsuanLai/Tutti/blob/main/crates/arc-lend/BENCHMARKS.md):
//! one run on a shared 4-vCPU VM, so its medians are indicative and its
//! tails are mostly the host's scheduler. In it:
//!
//! - **No arc-lend reader allocated or freed**, in any scenario.
//! - **A lend is flat in the reader count**: 41-42 ns p50 per block (a lend,
//!   a sum over 16 `u64`s, and the drop) at one to four readers with no
//!   publisher, against 51 ns for `arc-swap` and 53 ns for `left-right`, on
//!   a 30 ns timer floor. Under a flat-out publisher, 42-57 ns.
//! - **Nested lends cost about one lend per level**: 127 ns for 8 levels and
//!   227 ns for 16 on one reader, against 191 ns and 656 ns for `arc-swap`,
//!   whose 8 debt slots per thread run out. Under a flat-out publisher on
//!   one reader, 8 levels took 420 ns against `arc-swap`'s 192 ns: each
//!   level misses on the pointer and on its own slot line.
//! - **A publish never waits on a reader**: with four readers and a
//!   flat-out publisher it made the most publishes of any cell measured,
//!   and its 1 kHz tails stayed in microseconds.
//! - **A publish scans every registered reader**: 138 ns p50 with one
//!   registered, 1343 ns with 1000 (`left-right`, which also scans its
//!   readers, 2365 ns; `arc-swap` 576 ns at any count).
//! - **Freeing the retired list is linear**: the publish that frees 30000
//!   values took what freeing them from a `Vec` did (2.4 ms).
//! - **A reader preempted mid-lend pins memory**: with two readers lending
//!   in a loop and a flat-out publisher, up to 2902 values were live at
//!   once on that VM, against 2-3 for the other cells, all freed once the
//!   reader ran again (see [`limitations`](super::limitations)).
//!
//! The micro-benchmark `benches/lend.rs` (criterion) measures one lend and
//! one `arc-swap` load, uncontended and against a publisher, for a quick
//! check after a change: `cargo bench -p arc-lend --bench lend`.
