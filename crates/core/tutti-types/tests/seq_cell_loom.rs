//! A `loom` model of [`SeqCell`]: the shipped code, whose atomics and fences
//! are loom's under the flag (see `rt/seq_cell.rs`'s `sync` module).
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p tutti-types --release --test seq_cell_loom
//! ```
//!
//! Each write stores one value in every word, so a copy mixing two writes
//! shows as unequal words. It asserts, over every interleaving of one writer
//! and one reader:
//!
//! - **no mixed copy**: a read returns `None` or the words of one write,
//!   never words of two;
//! - **no going back**: two reads in program order never return an older
//!   write after a newer one;
//! - **the last write is read** on the writer's thread, after both writes.
//!
//! # Mutation record
//!
//! Each was applied to `rt/seq_cell.rs` and the model failed, both at
//! `LOOM_MAX_PREEMPTIONS=3` and unbounded (about a second and a half):
//!
//! - the writer's `Release` fence deleted → "a mixed copy: [1, 2]";
//! - the reader's `Acquire` fence deleted → "a mixed copy: [1, 2]";
//! - the reader's re-check of the sequence deleted (the copy returned as
//!   loaded) → "a mixed copy: [1, 2]";
//! - the reader not refusing an odd sequence (copying mid-write) → "a mixed
//!   copy: [0, 0]" (words no write stored);
//! - the writer's closing store weakened to `Relaxed` → the same;
//! - the reader's first sequence load weakened to `Relaxed` → the same.

#![cfg(loom)]

use std::sync::Arc;

use loom::thread;
use tutti_types::SeqCell;

/// The writes, in order. Words are the write's index, so 0 is "none".
const WRITES: [u64; 2] = [1, 2];

/// The write a copy came from, or a panic on a mixed one.
fn which(copy: Option<[u64; 2]>) -> u64 {
    match copy {
        None => 0,
        Some([a, b]) if a == b && WRITES.contains(&a) => a,
        // Unequal words, or words no write stored.
        Some(words) => panic!("a mixed copy: {words:?}"),
    }
}

/// The writer is the model's main thread and the reader a spawned one, not
/// the other way round: with a spawned writer whose first access is the
/// sequence load, loom 0.7 explored a single interleaving (the reader ran to
/// completion first) and every mutation below passed. Measured with an
/// iteration counter: one execution that way, tens of thousands this way.
#[test]
fn a_reader_never_sees_a_mixed_or_older_copy() {
    loom::model(|| {
        let cell = Arc::new(SeqCell::<2>::new());
        let reader = {
            let cell = Arc::clone(&cell);
            thread::spawn(move || {
                let first = which(cell.read());
                let second = which(cell.read());
                (first, second)
            })
        };
        for w in WRITES {
            cell.write([w, w]);
        }
        let (first, second) = reader.join().unwrap();
        // A `None` is a read that gave up, not an older write: only two
        // `Some`s are ordered.
        if first != 0 && second != 0 {
            assert!(second >= first, "a read went back from {first} to {second}");
        }
        assert_eq!(cell.read(), Some([2, 2]), "the last write is read");
    });
}
