//! A `loom` model of [`AudioThreadCell`]'s always-on borrow check: the
//! shipped code, whose state word is loom's atomic under the flag (see
//! `rt/cell.rs`'s `sync` module).
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p tutti-types --release --test cell_loom
//! ```
//!
//! The cell holds a `loom::cell::UnsafeCell`: each granted mutable borrow
//! writes it through `with_mut`, each granted shared borrow reads it through
//! `with`. Loom reports any write not ordered by happens-before against
//! another access, so over every interleaving the models assert:
//!
//! - **no aliasing**: two mutable borrows, or a shared and a mutable one,
//!   never overlap (the word refuses the second);
//! - **readers share**: a shared borrow taken while another is live is
//!   granted;
//! - **publication**: a borrow granted after another's guard dropped sees what
//!   it wrote, and a writer granted after readers dropped is ordered after
//!   their reads (the releases' `Release` pairs with the acquires' `Acquire`,
//!   through the release sequence of the readers' decrements);
//! - **release**: a borrower refused while another holds the cell is granted
//!   once that guard drops, and the cell is free once every guard has.
//!
//! # Mutation record
//!
//! Each was applied to `rt/cell.rs`, and the models named failed:
//!
//! - `try_acquire_writer` always granting (the check compiled out, as it was
//!   in release builds) → three models: "Causality violation: Concurrent
//!   write accesses to `UnsafeCell`" (and read/write);
//! - the writer's compare-exchange and store both `Relaxed` → the same three
//!   (the bit still excludes, but the next borrow is not ordered after the
//!   last's accesses);
//! - `release_writer` a no-op → `a_refused_writer_later_succeeds` ("Model
//!   exceeded maximum number of branches": the retry never succeeds),
//!   `two_writers_…` ("free once every guard dropped") and
//!   `a_reader_and_a_writer_never_overlap`;
//! - `release_reader`'s decrement `Relaxed` → `a_writer_follows_readers` and
//!   `a_reader_and_a_writer_never_overlap`: "Concurrent read and write
//!   accesses to `UnsafeCell`" (the writer is not ordered after the read);
//! - `try_acquire_reader` ignoring the writer bit → the same two: a
//!   causality violation;
//! - `try_acquire_reader` exclusive (a compare-exchange from 0 to 1) →
//!   `two_readers_are_both_admitted` ("a second reader must be admitted")
//!   and `a_writer_follows_readers`.
//!
//! # Why compare-exchanges
//!
//! The first version took the writer bit with `swap(true, Acquire)`, and the
//! first model failed it with "free once every guard dropped": a refused swap
//! still *writes*, and loom 0.7 leaves that write unordered against the
//! holder's later release, so after both threads finish it lets the next
//! borrow read the refused write. Under the C++ model that read is impossible
//! (the refused RMW sits right after the holder's acquire in modification
//! order, before its release), so it was loom's imprecision, not a bug; but a
//! compare-exchange writes nothing when refused and is no dearer, so both
//! acquires use one (the reader's in a loop, rather than `atomic_refcell`'s
//! unconditional `fetch_add`, for the same reason) and the models check the
//! code as shipped.

#![cfg(loom)]

use std::sync::Arc;

use loom::cell::UnsafeCell;
use loom::thread;
use tutti_types::AudioThreadCell;

/// The tracked payload. `Sync` by assertion, so the shared borrows (which
/// need `T: Sync`) are open to it: whether that assertion holds is exactly
/// what loom checks, through the accesses below.
struct Tracked(UnsafeCell<u32>);
// SAFETY: the model's subject. Every access goes through `with`/`with_mut`
// under a borrow, and loom fails the model if two of them race.
unsafe impl Sync for Tracked {}

fn cell() -> Arc<AudioThreadCell<Tracked>> {
    Arc::new(AudioThreadCell::new(Tracked(UnsafeCell::new(0))))
}

/// Borrow mutably, and if granted, bump the count inside the borrow. Returns
/// whether the borrow was granted.
fn bump(cell: &AudioThreadCell<Tracked>) -> bool {
    match cell.try_borrow_mut() {
        Some(guard) => {
            // SAFETY: loom tracks this access; the guard is the claim that
            // nothing else touches the value, and the model checks it.
            guard.0.with_mut(|p| unsafe { *p += 1 });
            true
        }
        None => false,
    }
}

/// Borrow shared, and if granted, read the count. `None` if refused.
fn read(cell: &AudioThreadCell<Tracked>) -> Option<u32> {
    let guard = cell.try_borrow()?;
    // SAFETY: as in `bump`, for a read.
    Some(guard.0.with(|p| unsafe { *p }))
}

#[test]
fn two_writers_never_alias_and_each_sees_the_last_write() {
    loom::model(|| {
        let cell = cell();
        let other = {
            let cell = Arc::clone(&cell);
            thread::spawn(move || bump(&cell))
        };
        let mine = bump(&cell);
        let theirs = other.join().unwrap();
        assert!(mine || theirs, "at least the first borrow is granted");

        let guard = cell
            .try_borrow_mut()
            .expect("free once every guard dropped");
        let count = guard.0.with(|p| unsafe { *p });
        assert_eq!(count, u32::from(mine) + u32::from(theirs));
    });
}

/// A writer refused while another holds the cell is granted once it drops:
/// each thread tries once, then retries until granted.
#[test]
fn a_refused_writer_later_succeeds() {
    loom::model(|| {
        let cell = cell();
        let worker = |cell: &AudioThreadCell<Tracked>| {
            let first = bump(cell);
            while !bump(cell) {
                thread::yield_now();
            }
            1 + u32::from(first)
        };
        let other = {
            let cell = Arc::clone(&cell);
            thread::spawn(move || worker(&cell))
        };
        let mine = worker(&cell);
        let theirs = other.join().unwrap();
        assert_eq!(read(&cell), Some(mine + theirs));
    });
}

/// A shared borrow taken while another is live is granted.
#[test]
fn two_readers_are_both_admitted() {
    loom::model(|| {
        let cell = cell();
        let held = cell.try_borrow().expect("nothing else borrows");
        let other = {
            let cell = Arc::clone(&cell);
            thread::spawn(move || read(&cell))
        };
        let theirs = other.join().unwrap();
        drop(held);
        assert_eq!(theirs, Some(0), "a second reader must be admitted");
    });
}

/// A writer granted after concurrent readers is ordered after their reads.
#[test]
fn a_writer_follows_readers() {
    loom::model(|| {
        let cell = cell();
        let reader = {
            let cell = Arc::clone(&cell);
            thread::spawn(move || read(&cell))
        };
        let mine = read(&cell);
        let wrote = bump(&cell);
        let theirs = reader.join().unwrap();
        assert_eq!(mine, Some(0), "nothing writes before this read");
        if let Some(seen) = theirs {
            assert!(seen <= 1);
        }
        assert!(wrote || theirs.is_some());
    });
}

/// A reader and a writer racing never overlap: whichever is second is
/// refused, or follows the first's release.
#[test]
fn a_reader_and_a_writer_never_overlap() {
    loom::model(|| {
        let cell = cell();
        let writer = {
            let cell = Arc::clone(&cell);
            thread::spawn(move || bump(&cell))
        };
        let seen = read(&cell);
        let wrote = writer.join().unwrap();
        assert!(wrote || seen.is_some(), "the first of the two is granted");
        if let Some(seen) = seen {
            assert!(seen <= 1);
        }
        assert_eq!(read(&cell), Some(u32::from(wrote)));
    });
}
