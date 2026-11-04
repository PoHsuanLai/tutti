//! The comparison table's claims about the other cells, checked by running
//! them rather than by citing their source.
//!
//! Each test drives one cell through the handshake that exposes its claim:
//! where a replaced value is freed (on the reader or on the writer), and
//! whether the writer waits for a reader. The source lines behind each claim
//! are cited in `benches/compare/contenders.rs`.
//!
//! Not checked here: basedrop's `replace` spinning while a reader is inside
//! `get` (`src/shared_cell.rs:117`). A reader is inside `get` only for its
//! increment, clone and decrement, so no handshake can hold it there without
//! patching basedrop.

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use arc_lend_compare::{CountingAlloc, Payload, begin_counting, end_counting};

mod common;
use common::Step;

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

/// A crossbeam-epoch reader runs the writer's deferred frees: every 128th
/// `pin()` on a thread collects (`src/internal.rs:450-453`), and collecting
/// drops whatever garbage has expired, whoever retired it.
///
/// Mutation: read through `crossbeam_epoch::unprotected()` instead of
/// `pin()` (sound here, since nothing frees while the reader runs alone). The
/// reader then never collects, and `payload_drops` reads 0.
#[test]
#[cfg_attr(miri, ignore = "threads and spinning")]
fn a_crossbeam_epoch_reader_frees_values_the_writer_retired() {
    let cell = Arc::new(crossbeam_epoch::Atomic::new(Payload::new(0)));
    // The writer, on a thread of its own: 65 publishes fill one 64-entry bag,
    // which goes to the global queue, and stay under the 128 pins that would
    // make the writer collect it itself. Its exit hands over the rest.
    {
        let cell = cell.clone();
        std::thread::spawn(move || {
            for i in 1..=65 {
                let guard = crossbeam_epoch::pin();
                let old = cell.swap(
                    crossbeam_epoch::Owned::new(Payload::new(i)),
                    Ordering::SeqCst,
                    &guard,
                );
                // SAFETY: out of the cell; freed once no thread pinned now is.
                unsafe { guard.defer_destroy(old) };
            }
        })
        .join()
        .unwrap();
    }
    let counts = {
        let cell = cell.clone();
        std::thread::spawn(move || {
            // Prewarmed: the first pin registers the thread, which allocates.
            drop(crossbeam_epoch::pin());
            begin_counting();
            for _ in 0..128 * 64 {
                let guard = crossbeam_epoch::pin();
                // SAFETY: never null; the guard keeps it alive.
                black_box(unsafe { cell.load(Ordering::Acquire, &guard).deref() }.touch());
            }
            end_counting()
        })
        .join()
        .unwrap()
    };
    assert!(counts.payload_drops > 0, "{counts:?}");
}

/// A haphazard reader frees nothing: a value it protects survives the
/// writer's reclaim, and is freed by the writer's next one.
///
/// Mutation: reset the reader's protection before it signals that it holds
/// the value: the writer's retire then frees the old value, and
/// `freed_while_held` reads 1.
#[test]
#[cfg_attr(miri, ignore = "threads and spinning")]
fn a_haphazard_reader_frees_nothing_and_its_value_survives_reclaim() {
    const HELD: u8 = 1;
    const RECLAIMED: u8 = 2;
    let cell = Arc::new(haphazard::AtomicPtr::from(Box::new(Payload::new(0))));
    let step = Arc::new(Step::default());
    let reader = {
        let (cell, step) = (cell.clone(), step.clone());
        // Acquired before the reader's counted window, as in the benchmark.
        let mut hp = haphazard::HazardPointer::new();
        std::thread::spawn(move || {
            begin_counting();
            let value = cell.safe_load(&mut hp).expect("never null");
            step.reach(HELD);
            step.wait(RECLAIMED);
            black_box(value.touch());
            hp.reset_protection();
            let counts = end_counting();
            drop(hp);
            counts
        })
    };
    step.wait(HELD);
    // A retire may reclaim too (on a timer that starts out due), so count
    // what it frees with what the eager reclaim does.
    let mut freed_while_held = 0;
    if let Some(old) = cell.swap(Box::new(Payload::new(1))) {
        // SAFETY: out of the cell, retired once.
        freed_while_held += unsafe { old.retire() };
    }
    freed_while_held += haphazard::Domain::global().eager_reclaim();
    step.reach(RECLAIMED);
    let counts = reader.join().unwrap();
    assert_eq!(freed_while_held, 0, "the protected value was freed");
    assert_eq!(counts, Default::default());
    assert_eq!(haphazard::Domain::global().eager_reclaim(), 1);
}

/// An arc-swap `Cache` reload on a reader thread, after the writer swapped
/// the value: the reload calls `load_full` and drops the old cached `Arc` on
/// the reader (`src/cache.rs:159-166`). `keep` is the graveyard: the writer
/// holds the replaced `Arc` across the reload. Returns the reader's counts.
fn arc_swap_cache_reload(keep: bool) -> arc_lend_compare::Counts {
    const BUILT: u8 = 1;
    const STORED: u8 = 2;
    let cell = Arc::new(arc_swap::ArcSwap::from_pointee(Payload::new(0)));
    let step = Arc::new(Step::default());
    let reader = {
        let (cell, step) = (cell.clone(), step.clone());
        std::thread::spawn(move || {
            // Built and prewarmed before the counted window, as in the
            // benchmark: the load makes this thread's debt node.
            let mut cache = arc_swap::Cache::new(cell.clone());
            drop(cell.load());
            step.reach(BUILT);
            step.wait(STORED);
            begin_counting();
            black_box(cache.load().touch());
            end_counting()
        })
    };
    step.wait(BUILT);
    let old = cell.swap(Arc::new(Payload::new(1)));
    let graveyard = keep.then_some(old);
    step.reach(STORED);
    let counts = reader.join().unwrap();
    drop(graveyard);
    counts
}

/// A cache's reload frees the old value on the reader when the writer has
/// dropped its own count: the design table's "reader frees" for `Cache`.
///
/// Mutation: keep the replaced `Arc` (`arc_swap_cache_reload(true)`):
/// `payload_drops` then reads 0.
#[test]
#[cfg_attr(miri, ignore = "threads and spinning")]
fn an_arc_swap_cache_reload_frees_on_the_reader() {
    let counts = arc_swap_cache_reload(false);
    assert_eq!(counts.payload_drops, 1, "{counts:?}");
}

/// With the benchmark's graveyard, the same reload frees nothing on the
/// reader.
///
/// Mutation: drop the replaced `Arc` before `STORED`
/// (`keep.then_some(old)` → `{ drop(old); None::<Arc<Payload>> }`): `counts`
/// then reads `frees: 2, payload_drops: 1`.
#[test]
#[cfg_attr(miri, ignore = "threads and spinning")]
fn an_arc_swap_cache_reload_behind_a_graveyard_frees_nothing() {
    assert_eq!(arc_swap_cache_reload(true), Default::default());
}

/// A triple_buffer reader frees nothing: its `read` swaps buffer indices,
/// and the writer's `write` drops what the back buffer held.
///
/// Mutation: make the reader's last read replace its buffer's value and drop
/// the old one (`drop(mem::replace(output.output_buffer_mut(), ..))`):
/// `counts` then reads `allocs: 2, frees: 2, payload_drops: 1`.
#[test]
#[cfg_attr(miri, ignore = "threads and spinning")]
fn a_triple_buffer_reader_frees_nothing() {
    const READ: u8 = 1;
    const WRITTEN: u8 = 2;
    let (mut input, mut output) = triple_buffer::triple_buffer(&Arc::new(Payload::new(0)));
    let step = Arc::new(Step::default());
    let reader = {
        let step = step.clone();
        std::thread::spawn(move || {
            begin_counting();
            for round in 0..4u8 {
                black_box(output.read().touch());
                step.reach(READ + 2 * round);
                step.wait(WRITTEN + 2 * round);
            }
            black_box(output.read().touch());
            end_counting()
        })
    };
    for round in 0..4u8 {
        step.wait(READ + 2 * round);
        input.write(Arc::new(Payload::new(u64::from(round) + 1)));
        step.reach(WRITTEN + 2 * round);
    }
    let counts = reader.join().unwrap();
    assert_eq!(counts, Default::default());
}

/// A left-right writer waits for a reader: a publish cannot finish while a
/// read guard taken before the previous publish is held.
///
/// Mutation: drop the guard before the writer starts: the writer then
/// finishes within the window, and `finished_while_held` reads true.
#[test]
#[cfg_attr(miri, ignore = "threads and sleeping")]
fn a_left_right_publish_waits_for_a_held_read() {
    #[derive(Clone)]
    struct Cell(u64);
    impl left_right::Absorb<u64> for Cell {
        fn absorb_first(&mut self, op: &mut u64, _: &Self) {
            self.0 = *op;
        }
        fn sync_with(&mut self, first: &Self) {
            self.0 = first.0;
        }
    }
    let (mut w, r) = left_right::new_from_empty::<Cell, u64>(Cell(0));
    w.publish();
    let guard = r.enter().expect("the writer is alive");
    let done = Arc::new(AtomicBool::new(false));
    let writer = {
        let done = done.clone();
        std::thread::spawn(move || {
            // The first swap moves readers to the other copy; the second has
            // to wait for every reader still in the old one.
            for v in 1..=2 {
                w.append(v);
                w.publish();
            }
            done.store(true, Ordering::Release);
            w
        })
    };
    std::thread::sleep(Duration::from_millis(100));
    let finished_while_held = done.load(Ordering::Acquire);
    black_box(guard.0);
    drop(guard);
    let w = writer.join().unwrap();
    assert!(
        !finished_while_held,
        "the publish did not wait for the reader"
    );
    assert_eq!(r.enter().expect("alive").0, 2);
    drop(w);
}
