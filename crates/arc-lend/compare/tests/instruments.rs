//! The benchmark's instrument, checked on its own: the counting allocator
//! counts the counting thread and only it, the payload counts its own drops,
//! and the instrument sees a free on a reader thread when one happens.
//!
//! These are deterministic (each handshake fixes the order), so they run in
//! the ordinary test suite, unlike the benchmark.
//!
//! A handshake inside a counted window is a [`Step`], never a
//! `std::sync::Barrier`: where std's mutex and condvar are pthread-backed and
//! boxed on first use (macOS), a barrier's first `wait` allocates on whichever
//! thread gets there first, and the reader was charged two allocations that
//! were the barrier's, not the cell's.

use std::hint::black_box;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

use arc_lend_compare::{begin_counting, end_counting, live_payloads, CountingAlloc, Payload};

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

/// A handshake that neither allocates nor frees on any platform: one thread
/// [`reach`](Step::reach)es a step, the other [`wait`](Step::wait)s for it.
///
/// Mutation: make `reach` allocate (`drop(black_box(Box::new(n)))` before the
/// store), as a lazily boxed barrier does on macOS.
/// `an_arc_lend_loan_frees_nothing_on_the_reader` then reads
/// `Counts { allocs: 1, frees: 1, .. }`,
/// `a_promoted_arc_swap_guard_frees_on_the_reader` reads `allocs: 1, frees: 3`
/// and `another_threads_allocations_are_not_counted` reads `allocs: 1`.
#[derive(Default)]
struct Step(AtomicU8);

impl Step {
    fn reach(&self, n: u8) {
        self.0.store(n, Ordering::Release);
    }
    fn wait(&self, n: u8) {
        while self.0.load(Ordering::Acquire) < n {
            std::hint::spin_loop();
        }
    }
}

/// Mutation: delete the `COUNTING.with(|c| c.set(true))` in `begin_counting`.
/// No thread is then ever counted, and both counts here read 0.
#[test]
#[cfg_attr(miri, ignore = "threads; the instrument is not unsafe-heavy")]
fn a_free_on_a_counting_thread_is_counted() {
    let counts = std::thread::spawn(|| {
        begin_counting();
        drop(black_box(Box::new([7u64; 4])));
        end_counting()
    })
    .join()
    .unwrap();
    assert_eq!(counts.allocs, 1, "{counts:?}");
    assert_eq!(counts.frees, 1, "{counts:?}");
}

/// Another thread's allocations are not this thread's: the counters are per
/// thread.
///
/// Mutation: make the counters process-wide (`static ALLOCS: AtomicU64` and
/// `static FREES: AtomicU64`, bumped whenever any thread is counting). The
/// quiet thread then reports the busy thread's thousand allocations.
#[test]
#[cfg_attr(miri, ignore = "threads; the instrument is not unsafe-heavy")]
fn another_threads_allocations_are_not_counted() {
    let go = Arc::new(Step::default());
    let done = Arc::new(AtomicBool::new(false));
    let quiet = {
        let (go, done) = (go.clone(), done.clone());
        std::thread::spawn(move || {
            begin_counting();
            go.reach(1);
            while !done.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }
            end_counting()
        })
    };
    go.wait(1);
    for _ in 0..1000 {
        drop(black_box(vec![1u8; 64]));
    }
    done.store(true, Ordering::Release);
    let counts = quiet.join().unwrap();
    assert_eq!(counts.allocs, 0, "{counts:?}");
    assert_eq!(counts.frees, 0, "{counts:?}");
}

/// Mutation: drop the `bump(&PAYLOAD_DROPS)` from `Payload::drop`.
/// `payload_drops` then reads 0.
#[test]
#[cfg_attr(miri, ignore = "threads; the instrument is not unsafe-heavy")]
fn a_payload_counts_itself_and_its_drop() {
    let (counts, during) = std::thread::spawn(|| {
        let p = Payload::new(1);
        let during = live_payloads();
        begin_counting();
        drop(p);
        (end_counting(), during)
    })
    .join()
    .unwrap();
    // Other tests run in their own process under nextest; under `cargo test`
    // they may hold payloads too, so compare against at least one.
    assert!(during >= 1);
    assert_eq!(counts.payload_drops, 1, "{counts:?}");
    assert_eq!(counts.frees, 1, "the payload's array, {counts:?}");
}

/// The instrument sees a reader-side free when one happens: arc-swap's
/// documented promotion (`src/docs/performance.rs:55-60`). A reader holds a
/// `load()` guard; the writer `store`s, which pays the guard's debt with an
/// owned count, and drops its own `Arc`; the reader's guard drop is then the
/// last and frees the payload on the reader.
///
/// Mutation: delete the flag in `begin_counting` (as above): `frees` and
/// `payload_drops` read 0 and this fails, which is the failure the
/// benchmark's tables would otherwise hide.
#[test]
#[cfg_attr(miri, ignore = "threads; the instrument is not unsafe-heavy")]
fn a_promoted_arc_swap_guard_frees_on_the_reader() {
    const HELD: u8 = 1;
    const STORED: u8 = 2;
    let cell = Arc::new(arc_swap::ArcSwap::from_pointee(Payload::new(0)));
    let step = Arc::new(Step::default());
    let reader = {
        let (cell, step) = (cell.clone(), step.clone());
        std::thread::spawn(move || {
            // Prewarmed: the thread's debt node exists before counting.
            black_box(cell.load().touch());
            begin_counting();
            let guard = cell.load();
            step.reach(HELD);
            step.wait(STORED);
            black_box(guard.touch());
            drop(guard);
            end_counting()
        })
    };
    step.wait(HELD);
    cell.store(Arc::new(Payload::new(1)));
    step.reach(STORED);
    let counts = reader.join().unwrap();
    assert_eq!(counts.payload_drops, 1, "{counts:?}");
    // The payload's array and the `Arc`'s allocation.
    assert_eq!(counts.frees, 2, "{counts:?}");
    assert_eq!(counts.allocs, 0, "{counts:?}");
}

/// The same handshake against arc-lend: the reader's loan drop frees nothing,
/// and the old value waits for the publisher's next publish.
///
/// Mutation: make `Reader::drop` reclaim (`self.cell.reclaim()` after
/// releasing the slot), as `arc-lend/tests/no_alloc.rs` records. The
/// reader's drop then frees the retired payload on the reader, and `counts`
/// reads a payload drop and its frees where it should read zeros.
#[test]
#[cfg_attr(miri, ignore = "threads; the instrument is not unsafe-heavy")]
fn an_arc_lend_loan_frees_nothing_on_the_reader() {
    const HELD: u8 = 1;
    const STORED: u8 = 2;
    let cell = Arc::new(arc_lend::ArcLend::new(Payload::new(0)));
    let step = Arc::new(Step::default());
    let reader = {
        // Registered here, on the control side, and moved to the reader.
        let (mut reader, step) = (cell.reader(), step.clone());
        std::thread::spawn(move || {
            begin_counting();
            let loan = reader.lend();
            step.reach(HELD);
            step.wait(STORED);
            black_box(loan.touch());
            drop(loan);
            drop(reader);
            end_counting()
        })
    };
    step.wait(HELD);
    cell.publish(Arc::new(Payload::new(1)));
    step.reach(STORED);
    let counts = reader.join().unwrap();
    assert_eq!(counts, Default::default());
    // The retired value is still waiting, freed by the next publish.
    assert_eq!(cell.retired_len(), 1);
    cell.publish(Arc::new(Payload::new(2)));
    assert_eq!(cell.retired_len(), 0);
}
