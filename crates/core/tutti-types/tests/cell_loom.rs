//! A `loom` model of [`AudioThreadCell`]'s always-on borrow flag: the shipped
//! code, whose flag is loom's atomic under the flag (see `rt/cell.rs`'s `sync`
//! module).
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p tutti-types --release --test cell_loom
//! ```
//!
//! The cell holds a `loom::cell::UnsafeCell`, and each borrow that is granted
//! writes it through `with_mut`. Loom reports any two such accesses that are
//! not ordered by a happens-before edge, so over every interleaving of the
//! threads below it asserts:
//!
//! - **no aliasing**: two granted borrows never overlap (the flag refuses the
//!   second);
//! - **publication**: a borrow granted after another's guard dropped sees what
//!   that borrow wrote (the `Release` store on drop pairs with the borrow's
//!   `Acquire` compare-exchange) — the reason that pair suffices, without a fence;
//! - **release**: once every guard has dropped, a borrow is granted.
//!
//! # Mutation record
//!
//! Each was applied to `rt/cell.rs` and the model failed:
//!
//! - `try_acquire` always granting (the check compiled out, as it was in
//!   release builds) → "Causality violation: Concurrent write accesses to
//!   `UnsafeCell`";
//! - the compare-exchange and the store both `Relaxed` → the same causality
//!   violation (the flag still excludes, but the second borrow's write is not
//!   ordered after the first's);
//! - `release` a no-op → "free once every guard dropped".
//!
//! # Why a compare-exchange
//!
//! The first version took the flag with `swap(true, Acquire)`, and this model
//! failed it with "free once every guard dropped": a refused swap still
//! *writes* `true`, and loom 0.7 leaves that write unordered against the
//! holder's later `store(false)`, so after both threads finish it lets the
//! next borrow read the refused write. Under the C++ model that read is
//! impossible (the refused RMW sits right after the holder's `true` in
//! modification order, before the holder's `false`), so it was loom's
//! imprecision, not a bug; but a compare-exchange, which is what
//! `atomic_refcell` uses, writes nothing when refused and is no dearer, so
//! the shipped code uses it and the model checks the code as shipped.

#![cfg(loom)]

use std::sync::Arc;

use loom::cell::UnsafeCell;
use loom::thread;
use tutti_types::AudioThreadCell;

/// Borrow, and if granted, bump the count inside the borrow. Returns whether
/// the borrow was granted.
fn bump(cell: &AudioThreadCell<UnsafeCell<u32>>) -> bool {
    match cell.try_borrow_mut() {
        Some(guard) => {
            // SAFETY: loom tracks this access; the guard is the claim that
            // nothing else touches the value, and the model checks it.
            guard.with_mut(|p| unsafe { *p += 1 });
            true
        }
        None => false,
    }
}

#[test]
fn two_threads_never_alias_and_each_sees_the_last_write() {
    loom::model(|| {
        let cell = Arc::new(AudioThreadCell::new(UnsafeCell::new(0u32)));
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
        let count = guard.with(|p| unsafe { *p });
        assert_eq!(count, u32::from(mine) + u32::from(theirs));
    });
}
