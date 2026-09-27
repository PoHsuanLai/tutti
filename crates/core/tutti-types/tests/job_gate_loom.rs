//! A `loom` model of [`JobGate`] — the shipped code, whose atomics and job
//! cell are loom's under the flag (see `rt/job_gate.rs`'s `sync` module).
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p tutti-types --release --test job_gate_loom
//! ```
//!
//! The job borrows a `loom::cell::UnsafeCell` owned by the caller's stack
//! frame, which the caller overwrites (`with_mut`) the moment `run` returns —
//! standing in for the borrow ending. A helper that entered the job reads it
//! (`with`). loom therefore reports, over every interleaving:
//!
//! - **no helper uses the job after `run` returns** — its read would race the
//!   caller's overwrite (the property the lifetime erasure rests on);
//! - **no helper reads a stale job** — the second `run` lends a different
//!   cell, and a helper that saw the first generation must not call into the
//!   second's job with the first's index twice (each helper serves a
//!   generation once; checked by the per-run counts);
//! - **indices are distinct** — each participant index runs at most once per
//!   call.
//!
//! # Mutation record
//!
//! Each was applied to `rt/job_gate.rs` and the model failed
//! (`LOOM_MAX_PREEMPTIONS=3`):
//!
//! - the caller's wait for `entered` in `Close::drop` deleted → a causality
//!   violation on the job cell (a helper reads after the overwrite);
//! - the helper's re-read of `state` after entering deleted → the same;
//! - either `SeqCst` fence (the caller's in `Close::drop`, the helper's in
//!   `help`) removed → the same;
//! - the exit's `Release` (`Leave::drop`) or the wait's `Acquire` weakened
//!   to `Relaxed` → the same;
//! - `help` made not to record the generation it served → "index 1 ran
//!   twice".

#![cfg(loom)]

use std::sync::Arc;

use loom::cell::UnsafeCell;
use loom::sync::atomic::{AtomicU32, Ordering};
use loom::thread;
use tutti_types::JobGate;

struct Borrowed {
    value: UnsafeCell<u32>,
    ran: [AtomicU32; 2],
}

// SAFETY (test): shared only through the gate, which is what loom checks.
unsafe impl Sync for Borrowed {}

#[test]
fn a_helper_never_outlives_the_call() {
    loom::model(|| {
        let gate = Arc::new(JobGate::new(2));
        let helper = {
            let gate = Arc::clone(&gate);
            thread::spawn(move || {
                let mut served = 0;
                for _ in 0..3 {
                    gate.help(&mut served);
                    thread::yield_now();
                }
            })
        };
        for round in 0..2u32 {
            let b = Borrowed {
                value: UnsafeCell::new(round),
                ran: [AtomicU32::new(0), AtomicU32::new(0)],
            };
            // Borrowed whole (not field by field), so the closure is `Sync`
            // through `Borrowed`'s impl.
            let bb = &b;
            gate.run(
                &move |i| {
                    // SAFETY (test): a shared read of the borrowed cell.
                    let v = bb.value.with(|p| unsafe { *p });
                    assert_eq!(v, round, "a job read another call's state");
                    assert_eq!(
                        bb.ran[i].fetch_add(1, Ordering::Relaxed),
                        0,
                        "index {i} ran twice"
                    );
                },
                || {},
            );
            // The borrow ends: overwrite, as a drop would.
            // SAFETY (test): exclusive, and loom checks it against every
            // helper read.
            b.value.with_mut(|p| unsafe { *p = u32::MAX });
            assert_eq!(b.ran[0].load(Ordering::Relaxed), 1);
        }
        helper.join().unwrap();
    });
}
