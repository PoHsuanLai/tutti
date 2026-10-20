//! The benchmark's instrument, checked on its own: the counting allocator
//! counts the counting thread and only it, the payload counts its own drops,
//! and the instrument sees a free on a reader thread when one happens.
//!
//! These are deterministic (each handshake fixes the order), so they run in
//! the ordinary test suite, unlike the benchmark.

use std::hint::black_box;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};

use arc_lend_compare::{begin_counting, end_counting, live_payloads, CountingAlloc, Payload};

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

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
    let go = Arc::new(Barrier::new(2));
    let done = Arc::new(AtomicBool::new(false));
    let quiet = {
        let (go, done) = (go.clone(), done.clone());
        std::thread::spawn(move || {
            begin_counting();
            go.wait();
            while !done.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }
            end_counting()
        })
    };
    go.wait();
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
    let cell = Arc::new(arc_swap::ArcSwap::from_pointee(Payload::new(0)));
    let held = Arc::new(Barrier::new(2));
    let stored = Arc::new(Barrier::new(2));
    let reader = {
        let (cell, held, stored) = (cell.clone(), held.clone(), stored.clone());
        std::thread::spawn(move || {
            // Prewarmed: the thread's debt node exists before counting.
            black_box(cell.load().touch());
            begin_counting();
            let guard = cell.load();
            held.wait();
            stored.wait();
            black_box(guard.touch());
            drop(guard);
            end_counting()
        })
    };
    held.wait();
    cell.store(Arc::new(Payload::new(1)));
    stored.wait();
    let counts = reader.join().unwrap();
    assert_eq!(counts.payload_drops, 1, "{counts:?}");
    // The payload's array and the `Arc`'s allocation.
    assert_eq!(counts.frees, 2, "{counts:?}");
    assert_eq!(counts.allocs, 0, "{counts:?}");
}

/// The same handshake against arc-lend: the reader's loan drop frees nothing,
/// and the old value waits for the publisher's next publish.
///
/// Mutation: make `Loan::drop` reclaim, as `arc-lend/tests/no_alloc.rs`
/// records (lock the control block, `take_unprotected`, drop the result, in
/// the slot branch). The loan's drop then frees the retired payload on the
/// reader, and `counts` reads one payload drop, three frees and one
/// allocation (the free list) where it should read zeros.
#[test]
#[cfg_attr(miri, ignore = "threads; the instrument is not unsafe-heavy")]
fn an_arc_lend_loan_frees_nothing_on_the_reader() {
    let cell = Arc::new(arc_lend::ArcLend::new(Payload::new(0)));
    let held = Arc::new(Barrier::new(2));
    let stored = Arc::new(Barrier::new(2));
    let reader = {
        let (cell, held, stored) = (cell.clone(), held.clone(), stored.clone());
        std::thread::spawn(move || {
            begin_counting();
            let loan = cell.lend();
            held.wait();
            stored.wait();
            black_box(loan.touch());
            drop(loan);
            end_counting()
        })
    };
    held.wait();
    cell.publish(Arc::new(Payload::new(1)));
    stored.wait();
    let counts = reader.join().unwrap();
    assert_eq!(counts, Default::default());
    // The retired value is still waiting, freed by the next publish.
    assert_eq!(cell.retired_len(), 1);
    cell.publish(Arc::new(Payload::new(2)));
    assert_eq!(cell.retired_len(), 0);
}
