//! Gate: a reader's code path (lend, and drop a loan) neither allocates nor
//! frees, on the slot path or the overflow path.
//!
//! Moved from tutti-types' `tests/rt_no_alloc.rs` with the cell.

// Under `--cfg loom` the cell's atomics are loom's, which panic outside a
// `loom::model`.
#![cfg(not(loom))]

use assert_no_alloc::AllocDisabler;

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// The whole reader side of `ArcLend` — slot lends, overflow lends past the
/// slot count, and dropping a loan of a value that has since been retired —
/// neither allocates nor frees.
///
/// This is a claim about the *code path*, which is deterministic and so can be
/// pinned by a gate: `Loan::drop` has no branch that runs a destructor. The
/// race-shaped half of the property (a publisher reclaiming concurrently) is
/// the loom model's job, `tests/loom.rs`, not this test's.
///
/// Mutation: make `Loan::drop` reclaim (call `take_unprotected` and drop the
/// result, in both the slot and the overflow branch). The last `held` drop
/// then frees the retired `Vec` inside the gate, and `assert_no_alloc` aborts.
#[test]
fn the_reader_side_neither_allocates_nor_frees() {
    use arc_lend::ArcLend;
    use std::sync::Arc;

    let cell = ArcLend::new(vec![1u32; 64]);
    // More live loans than any sane slot count, so the overflow path runs too.
    let mut held = Vec::with_capacity(64);
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..32 {
            held.push(cell.lend());
        }
    });
    // Retire the value every `held` read points at. `publish` allocates the
    // new value and may grow the retirement list; that is the control side.
    cell.publish(Arc::new(vec![2u32; 64]));
    assert!(held.iter().all(|r| r[0] == 1));

    assert_no_alloc::assert_no_alloc(|| {
        // A fresh lend sees the new value, and dropping the last readers of
        // the retired one frees nothing.
        assert_eq!(cell.lend()[0], 2);
        // Newest first, so the overflow readers go before the slot readers
        // and the last drop is the one a reclaiming drop would free on.
        while let Some(r) = held.pop() {
            drop(r);
        }
    });
}
