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
//!   cell afterwards). `the_caller_sees_every_task_through_the_sinks`
//!   checks this alone: its tasks read nothing, two of its five are sinks,
//!   and only the sinks are counted for termination, so a non-sink task's
//!   write reaches the caller only through the counter chain to a sink
//!   (`rt/task_graph.rs`, "Ordering");
//! - **`begin` re-arms the counters**: a second block runs every task
//!   again;
//! - **a helper may leave early** (under loom its budget is a count: it
//!   leaves on its third wait with nothing to take, whatever the
//!   `BlockWaits`), and the caller still runs every task and sees every
//!   write.
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
//!   both models (the caller's final read of a cell);
//! - one sink left uncounted (`remaining` starting at the sink count less
//!   one, and the last task, a sink in both graphs, not decrementing it) →
//!   both models (the caller returns before that sink's write);
//! - the successor decrement's `AcqRel` weakened to `Release` also fails
//!   `the_caller_sees_every_task_through_the_sinks`, which reads no
//!   predecessor's cell: the termination proof rests on that chain. With
//!   every task counted (the earlier design), the same mutation passed that
//!   model and failed only `diamond_two_participants`;
//! - `begin`'s counter re-arm removed → `diamond_two_participants` (the
//!   second block's counters wrap and never reach zero, so the caller
//!   spins; loom fails it with "Model exceeded maximum number of
//!   branches"). Re-run after the caller's wait became `SpinThenYield`
//!   (`rt/spin.rs`, budget 2 under loom), and after the termination count
//!   became the sinks': it still fails the same way.
//!
//! The record was re-run in full when the termination count became the
//! sinks' and the helpers gained completion counters; every mutation above
//! still fails.
//! When the in-block waits became per-pool budgets (`BlockWaits`, a count
//! under loom as before, so no ordering changed), the `pop` `Acquire`, the
//! `remaining` `Release`, the successor decrement's `AcqRel` and the
//! `begin` re-arm mutations were re-run: each still fails as recorded.
//!
//! The caller's spin-then-yield is a scheduling change, not an ordering
//! one: its progress signal (the `remaining` load the exit test makes, and
//! the helpers' `Relaxed` completion counters, loom's atomics here too)
//! orders nothing, and loom's `spin_loop` and `yield_now` both just
//! schedule another thread. So removing the yield, or the counters, is not a
//! mutation this model can see; the unit tests
//! `the_caller_yields_while_a_helper_holds_a_task`,
//! `a_helpers_every_task_moves_its_completion_counter` and `rt::spin`'s
//! catch those.

#![cfg(loom)]

use std::sync::Arc;

use loom::cell::UnsafeCell;
use loom::thread;
use tutti_types::{BlockWaits, Participant, TaskGraph};

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
                thread::spawn(move || {
                    g.work(Participant::Helper(1), BlockWaits::DEFAULT, |t| {
                        run_task(&cells, t, block)
                    })
                })
            };
            shared.work(Participant::Caller, BlockWaits::DEFAULT, |t| {
                run_task(&cells, t, block)
            });
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

/// 0 → {1, 2}, 1 → {3, 4}, 2 → 3: two sinks (3 and 4), and tasks that read
/// nothing, so the only thing ordering a task's write before the caller's
/// read after `work` is the termination count.
const SINKS_INITIAL: [u32; 5] = [0, 1, 1, 2, 1];
const SINKS_OFFSETS: [u32; 6] = [0, 2, 4, 5, 5, 5];
const SINKS_TARGETS: [u32; 5] = [1, 2, 3, 4, 3];

#[test]
fn the_caller_sees_every_task_through_the_sinks() {
    loom::model(|| {
        let mut g = TaskGraph::new(&SINKS_INITIAL, &SINKS_OFFSETS, &SINKS_TARGETS);
        let cells = Arc::new(Cells((0..5).map(|_| UnsafeCell::new(0)).collect()));
        g.begin();
        let g = Arc::new(g);
        let write = |cells: &Cells, t: u32| {
            // SAFETY (test): the task's own cell.
            cells.0[t as usize].with_mut(|x| unsafe { *x += 1 });
        };
        let helper = {
            let (g, cells) = (Arc::clone(&g), Arc::clone(&cells));
            thread::spawn(move || {
                g.work(Participant::Helper(1), BlockWaits::DEFAULT, |t| {
                    write(&cells, t)
                })
            })
        };
        g.work(Participant::Caller, BlockWaits::DEFAULT, |t| {
            write(&cells, t)
        });
        assert!(g.done());
        for (t, c) in cells.0.iter().enumerate() {
            // SAFETY (test): after `work` returned on this participant.
            assert_eq!(c.with(|x| unsafe { *x }), 1, "task {t}");
        }
        helper.join().unwrap();
    });
}
