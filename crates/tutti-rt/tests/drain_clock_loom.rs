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
