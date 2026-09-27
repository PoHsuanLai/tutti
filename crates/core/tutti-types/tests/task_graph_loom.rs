//! A `loom` model of [`TaskGraph`]'s dispatch — the shipped code, whose
//! atomics are loom's under the flag (see `rt/task_graph.rs`'s `sync`
//! module).
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p tutti-types --release --test task_graph_loom
//! ```
//!
//! Each task writes its own `loom::cell::UnsafeCell` after reading each of
//! its predecessors', so loom checks, over every interleaving of two
//! participants:
//!
//! - **a task runs after its predecessors, and sees their writes** (a read
//!   not ordered after the write is a causality violation);
//! - **every task runs exactly once per block** (asserted from the cells);
//! - **the caller sees every write once `work` returns** (it reads every
//!   cell afterwards);
//! - **the counters reset themselves**: a second block, after `begin`, runs
//!   every task again.
//!
//! # Mutation record
//!
//! Each was applied to `rt/task_graph.rs` and a model failed
//! (`LOOM_MAX_PREEMPTIONS=3`):
//!
//! - the successor decrement's `AcqRel` weakened to `Release` →
//!   `diamond_two_participants` (the join task reads a predecessor's cell
//!   unordered);
//! - `push`'s `Release` store weakened to `Relaxed` →
//!   `diamond_two_participants`;
//! - `pop`'s `Acquire` load weakened to `Relaxed` → `diamond_two_participants`;
//! - `pop`'s `EMPTY` check removed → `diamond_two_participants` (an index out
//!   of bounds when the popped reservation is not yet stored);
//! - the `remaining` decrement's `Release` weakened to `Relaxed` →
//!   `diamond_two_participants` (the caller's final read of a cell);
//! - the self-reset removed → `diamond_two_participants` (the second block
//!   never runs the join).

#![cfg(loom)]

use std::sync::Arc;

use loom::cell::UnsafeCell;
use loom::thread;
use tutti_types::{Participant, TaskGraph};

struct Cells(Vec<UnsafeCell<u32>>);

// SAFETY (test): tasks touch cells only as the graph orders them, which is
// what loom checks.
unsafe impl Sync for Cells {}
unsafe impl Send for Cells {}

/// 0 → {1, 2} → 3.
const PREDS: [&[u32]; 4] = [&[], &[0], &[0], &[1, 2]];

fn run_task(cells: &Cells, t: u32, block: u32) {
    for &p in PREDS[t as usize] {
        // SAFETY (test): a shared read loom orders against the write.
        let v = cells.0[p as usize].with(|x| unsafe { *x });
        assert_eq!(v, block, "task {t} ran before its predecessor {p}");
    }
    // SAFETY (test): the task's own cell.
    cells.0[t as usize].with_mut(|x| unsafe { *x += 1 });
}

#[test]
fn diamond_two_participants() {
    loom::model(|| {
        let mut g = TaskGraph::new(&[0, 1, 1, 2], &[0, 2, 3, 4, 4], &[1, 2, 3, 3]);
        let cells = Arc::new(Cells((0..4).map(|_| UnsafeCell::new(0)).collect()));
        for block in 1..=2u32 {
            g.begin();
            let shared = Arc::new(g);
            let helper = {
                let (g, cells) = (Arc::clone(&shared), Arc::clone(&cells));
                thread::spawn(move || g.work(Participant::Helper, |t| run_task(&cells, t, block)))
            };
            shared.work(Participant::Caller, |t| run_task(&cells, t, block));
            assert!(shared.done());
            for c in &cells.0 {
                // SAFETY (test): after `work` returned on this participant.
                assert_eq!(c.with(|x| unsafe { *x }), block);
            }
            helper.join().unwrap();
            g = Arc::try_unwrap(shared).expect("the helper dropped its handle");
        }
    });
}
