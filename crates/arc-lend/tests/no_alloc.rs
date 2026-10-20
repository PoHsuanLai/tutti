//! Gate: a reader's code path (lend, drop a loan, drop the reader) neither
//! allocates nor frees.
//!
//! Moved from tutti-types' `tests/rt_no_alloc.rs` with the cell.

// Under `--cfg loom` the cell's atomics are loom's, which panic outside a
// `loom::model`.
#![cfg(not(loom))]

use assert_no_alloc::AllocDisabler;

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// The whole reader side of `ArcLend` (lends through many registered
/// readers, dropping a loan of a value that has since been retired, and
/// dropping the readers themselves) neither allocates nor frees.
///
/// Registration is the control side and happens outside the gate. This is a
/// claim about the *code path*, which is deterministic and so can be pinned
/// by a gate: neither `Loan::drop` nor `Reader::drop` has a branch that runs
/// a destructor of a published value. The race-shaped half of the property
/// (a publisher reclaiming concurrently) is the loom model's job,
/// `tests/loom.rs`, not this test's.
///
/// Mutation: make `Reader::drop` reclaim (`self.cell.reclaim();` before its
/// store). The first reader drop inside the second gate then frees the
/// retired `Vec`, and `assert_no_alloc` aborts.
#[test]
fn the_reader_side_neither_allocates_nor_frees() {
    use arc_lend::ArcLend;
    use std::sync::Arc;

    let cell = Arc::new(ArcLend::new(vec![1u32; 64]));
    // More readers than one block of slots, so a lend through a slot of a
    // later block runs too.
    let mut readers: Vec<_> = (0..40).map(|_| cell.reader()).collect();
    let mut sum = 0;
    assert_no_alloc::assert_no_alloc(|| {
        for reader in &mut readers {
            sum += reader.lend()[0];
        }
    });
    assert_eq!(sum, 40);

    // Every reader parks a loan of the first value, which is then retired.
    // `publish` allocates the new value and may grow the retirement list;
    // that is the control side.
    let mut held: Vec<_> = readers.iter_mut().map(|r| r.lend()).collect();
    cell.publish(Arc::new(vec![2u32; 64]));
    assert_eq!(cell.retired_len(), 1);
    assert!(held.iter().all(|r| r[0] == 1));

    let mut fresh = cell.reader();
    let mut fresh_value = 0;
    assert_no_alloc::assert_no_alloc(|| {
        // A fresh lend sees the new value, and dropping every loan of the
        // retired one, then every reader, frees nothing.
        fresh_value = fresh.lend()[0];
        while let Some(loan) = held.pop() {
            drop(loan);
        }
    });
    assert_eq!(fresh_value, 2);
    // The emptied `Vec`'s buffer is the test's, not the cell's: freed here.
    drop(held);
    assert_no_alloc::assert_no_alloc(|| {
        // Dropping a reader drops its `Arc` of the cell, which is not the
        // last (`cell` is), so this is one store and a decrement each.
        while let Some(reader) = readers.pop() {
            drop(reader);
        }
    });
    // The retired value is still in the list: only the publisher frees it.
    assert_eq!(cell.retired_len(), 1);
    assert_eq!(cell.reclaim(), 0);
}
