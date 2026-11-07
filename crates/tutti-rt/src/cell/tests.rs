extern crate std;
use super::*;

/// Sequential borrows on one thread: each guard's drop frees the cell.
///
/// Mutation (run): `release_writer` a no-op (the bit never clears) → the
/// `borrow` after the first guard drops panics → fails.
#[test]
fn borrow_mut_from_same_thread() {
    let cell = AudioThreadCell::new(42u32);
    *cell.borrow_mut() = 100;
    assert_eq!(*cell.borrow(), 100);
    *cell.borrow_mut() += 1;
    assert_eq!(*cell.borrow(), 101);
}

/// After one thread's borrow drops, another thread may borrow. This is
/// the CoreAudio callback-migration case an owner-pinning check rejects.
///
/// Mutation (run): `release_writer` a no-op → the spawned thread's borrow
/// panics → fails.
#[test]
fn sequential_borrows_across_threads_are_ok() {
    use std::sync::Arc;
    let cell = Arc::new(AudioThreadCell::new(0u32));
    *cell.borrow_mut() = 7;

    let cell2 = Arc::clone(&cell);
    std::thread::spawn(move || {
        *cell2.borrow_mut() = 9;
    })
    .join()
    .unwrap();

    assert_eq!(*cell.borrow(), 9);
}

/// A second borrow while one is live panics, in every build: this test is
/// not `cfg(debug_assertions)`, and `--release` runs it with the check
/// that makes the cell `Sync`.
///
/// # Two barriers, and why neither is a sleep
///
/// The property needs the main thread's borrow attempt to fall strictly
/// *inside* the other thread's live guard. A sleep would make the test a
/// bet: if the main thread is descheduled past it the guard is already
/// gone, `borrow_mut` succeeds, and the test proves nothing about the check
/// it names.
///
/// Two barriers close the window without any duration at all. `taken` is
/// released after the guard exists, so the main thread cannot attempt its
/// borrow too early; `attempted` is released after the attempt has returned,
/// so the holder cannot drop the guard too early. Between them the ordering
/// is enforced rather than hoped for, and the test has no timing to tune.
///
/// Mutation (run): gate `try_acquire_writer`'s compare-exchange behind
/// `cfg(debug_assertions)` (always succeeding without it), the shape the
/// cell had → `cargo nextest run --release -p tutti-rt -E
/// 'test(concurrent_borrows_panic)'` fails (no panic: two `&mut u32`
/// alias), while the debug run still passes.
#[test]
fn concurrent_borrows_panic() {
    use std::sync::{Arc, Barrier};
    let cell = Arc::new(AudioThreadCell::new(0u32));
    // "The guard is live" and "the second borrow has been attempted". Two,
    // not one: a single barrier orders only the first of those two edges.
    let taken = Arc::new(Barrier::new(2));
    let attempted = Arc::new(Barrier::new(2));

    let cell2 = Arc::clone(&cell);
    let taken2 = Arc::clone(&taken);
    let attempted2 = Arc::clone(&attempted);
    let other = std::thread::spawn(move || {
        let _g = cell2.borrow_mut();
        taken2.wait();
        // Hold the guard until the main thread's attempt has completed.
        attempted2.wait();
    });

    taken.wait();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _g = cell.borrow_mut();
    }));
    // The non-panicking form refuses too, and both `borrow` forms do.
    let tried = cell.try_borrow_mut().is_none();
    let shared = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _g = cell.borrow();
    }));
    attempted.wait();

    other.join().unwrap();
    assert!(
        result.is_err(),
        "expected panic from concurrent borrow while another borrow was live"
    );
    assert!(tried, "try_borrow_mut must refuse while a borrow is live");
    assert!(shared.is_err(), "borrow must panic while a borrow is live");

    // The guard is gone now, so the cell must be borrowable again. Without
    // this the test would also pass against a cell that had panicked itself
    // into a permanently-locked state.
    assert_eq!(*cell.borrow(), 0);
}

/// Shared borrows on two threads are both granted at once, and a mutable
/// borrow is refused while either is live.
///
/// The same two-barrier shape as `concurrent_borrows_panic`: the main
/// thread's borrow falls strictly inside the other thread's.
///
/// Mutation (run): `try_acquire_reader` as a compare-exchange from 0 to
/// 1 (shared borrows exclusive, as before this cell counted readers) →
/// the main thread's `try_borrow` is refused → fails.
#[test]
fn concurrent_shared_borrows_are_both_granted() {
    use std::sync::{Arc, Barrier};
    let cell = Arc::new(AudioThreadCell::new(5u32));
    let taken = Arc::new(Barrier::new(2));
    let attempted = Arc::new(Barrier::new(2));

    let cell2 = Arc::clone(&cell);
    let (taken2, attempted2) = (Arc::clone(&taken), Arc::clone(&attempted));
    let other = std::thread::spawn(move || {
        let r = cell2.borrow();
        taken2.wait();
        attempted2.wait();
        *r
    });

    taken.wait();
    let mine = cell.try_borrow().map(|r| *r);
    let writer_refused = cell.try_borrow_mut().is_none();
    attempted.wait();

    assert_eq!(other.join().unwrap(), 5);
    assert_eq!(mine, Some(5), "a second reader must be admitted");
    assert!(
        writer_refused,
        "a writer must be refused while readers are live"
    );
    *cell.borrow_mut() = 6;
    assert_eq!(*cell.borrow(), 6);
}

/// A reader is refused while a writer is live, writes nothing when
/// refused, and the last reader's drop frees the cell for a writer.
///
/// Mutations (run): `release_reader` a no-op → the `borrow_mut` after
/// the readers drop panics → fails; the reader check against
/// `WRITER - 1` dropped (a reader admitted beside the writer) →
/// `try_borrow` returns `Some` while `w` is live → fails.
#[test]
fn readers_and_a_writer_exclude_each_other() {
    let cell = AudioThreadCell::new(1u32);
    let w = cell.borrow_mut();
    assert!(cell.try_borrow().is_none());
    assert!(
        cell.try_borrow().is_none(),
        "a refused reader changed the word"
    );
    drop(w);

    let a = cell.borrow();
    let b = cell.borrow();
    assert_eq!(*a + *b, 2);
    assert!(cell.try_borrow_mut().is_none());
    drop(a);
    assert!(cell.try_borrow_mut().is_none(), "one reader is still live");
    drop(b);
    *cell.borrow_mut() = 3;
    assert_eq!(*cell.borrow(), 3);
}

/// The panic names the call site that broke the contract, through
/// `#[track_caller]` on `borrow_mut`.
///
/// Mutation (run): drop `#[track_caller]` from `borrow_mut` → the message
/// names `borrow_mut`'s own line in this file → fails.
#[test]
fn the_panic_names_the_second_borrow() {
    let cell = AudioThreadCell::new(0u32);
    let _held = cell.borrow_mut();
    let line = line!() + 2;
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _g = cell.borrow_mut();
    }))
    .expect_err("a nested borrow panics");
    let msg = panic
        .downcast_ref::<std::string::String>()
        .expect("a formatted panic message");
    assert!(msg.contains("concurrent borrow detected"), "{msg}");
    let site = std::format!("{}:{line}:", file!());
    assert!(msg.contains(&site), "expected {site} in {msg}");
}

/// A refused `try_borrow_mut` leaves the live borrow's flag alone, and a
/// borrow after the guard drops succeeds.
///
/// Mutation (run): build the guard with `bool::then_some` (eagerly, so
/// the refused attempt's unused guard drops and clears the writer bit) →
/// the second attempt gets `Some` while `held` is live → fails.
#[test]
fn a_refused_try_borrow_keeps_the_live_borrow() {
    let cell = AudioThreadCell::new(1u32);
    let held = cell.borrow_mut();
    assert!(cell.try_borrow_mut().is_none());
    assert!(
        cell.try_borrow_mut().is_none(),
        "a refused attempt released the live borrow"
    );
    drop(held);
    *cell.try_borrow_mut().expect("free after the drop") += 1;
    assert_eq!(*cell.borrow(), 2);
}
