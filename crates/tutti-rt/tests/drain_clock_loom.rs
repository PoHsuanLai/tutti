//! A `loom` model of [`drain_clock()`]: the shipped code, whose atomics are
//! loom's under the flag (see `src/drain_clock.rs`'s `sync` module).
//!
//! ```text
//! cargo test -p tutti-rt --release --test drain_clock_loom   (with the loom cfg flag set)
//! ```
//!
//! The control thread sends one write (a `Release` store to a slot, the queue's
//! push) and marks. The audio thread runs three blocks: each begins a drain,
//! loads the slot (`Acquire`, the queue's pop) and logs what it saw. Over every
//! interleaving it asserts that when the control thread's `two_since` answers
//! true, some drain the audio thread has logged saw the write: a record entry
//! is dropped only after a drain that must have seen it has begun.
//!
//! # Mutation record
//!
//! Each was applied to `src/drain_clock.rs`; the model failed as noted. The
//! first row is what settles `+ 2`.
//!
//! - `two_since` as `>= mark + 1` (instead of `+ 2`) → "dropped before a drain
//!   saw the write": the drain the mark counted read the slot before the write,
//!   and the next count is already enough. This is why it is two;
//! - no fence in `DrainBeat::begin` → the same (the store-load pair is broken);
//! - no fence in `DrainWatch::mark` → the same;
//! - `begin` with `Relaxed` instead of `Release` → the same (the log of an
//!   earlier drain is not ordered before the count);
//! - the test marking before it sends → the same.
//!
//! The model runs 1,148 executions (`LOOM_LOG=info` shows loom's own count; the
//! test prints the number of times the closure ran).

#![cfg(loom)]

use loom::sync::Arc;
use loom::sync::atomic::{AtomicU32, Ordering};
use loom::thread;
use tutti_rt::drain_clock;

const BLOCKS: usize = 3;

/// Executions loom ran, to report.
static RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[test]
fn a_write_is_dropped_only_after_a_drain_that_saw_it() {
    loom::model(|| {
        RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let slot = Arc::new(AtomicU32::new(0));
        let log: Arc<[AtomicU32; BLOCKS]> = Arc::new(std::array::from_fn(|_| AtomicU32::new(0)));
        let (mut beat, watch) = drain_clock();

        let audio = {
            let (slot, log) = (slot.clone(), log.clone());
            thread::spawn(move || {
                for entry in log.iter() {
                    beat.begin();
                    let seen = slot.load(Ordering::Acquire);
                    // The next `begin` comes after this store and orders it.
                    entry.store(seen, Ordering::Relaxed);
                }
            })
        };

        slot.store(1, Ordering::Release);
        let mark = watch.mark();
        // Asked while the audio thread runs, as a save would.
        for _ in 0..2 {
            if watch.two_since(mark) {
                let saw = log.iter().any(|e| e.load(Ordering::Relaxed) == 1);
                assert!(saw, "dropped before a drain saw the write");
            }
        }
        audio.join().unwrap();
    });
    eprintln!(
        "drain_clock model: {} executions",
        RUNS.load(std::sync::atomic::Ordering::Relaxed)
    );
}
