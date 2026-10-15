//! A `loom` model of [`SeqWriter`] and [`SeqReader`]: the shipped code, whose atomics and fences
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
//! Each was applied to `rt/seq_cell.rs` (re-run against `SeqWriter`) and the
//! model failed, both at
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

use loom::thread;
use tutti_types::{SeqReader, SeqWriter};

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

/// The writer is the model's main thread and the reader a spawned one. That
/// is not a rule about writers; it works around a loom 0.7 limit. Loom keeps
/// one `last_access` per atomic (`rt/atomic.rs`), so when a thread loads an
/// atomic and then stores to it (or RMWs it), its own load becomes the last
/// access, and another thread's earlier load of that atomic drops out of the
/// race check: loom never schedules the other order. A spawned thread doing
/// `load` then `store` against a main thread doing one `load` runs in one
/// execution; the same with the roles swapped runs in six; a spawned thread
/// that only loads, against a main thread that stores, explores fully. The
/// first version of the writer loaded the sequence before storing it, so as
/// the spawned thread it hid the reader's loads and every mutation below
/// passed. Whichever thread loads then stores must be main, or the model
/// must check its own execution count. (The same limit under-explores
/// `job_gate`, `rt_publish`'s `two_publishers_and_a_reader` and several
/// `pos_ring` models by 4 to 24%; mutation sweeps there found nothing
/// hidden, and the fix is a separate change.)
fn reader_thread(reader: SeqReader<2>) -> thread::JoinHandle<(u64, u64)> {
    thread::spawn(move || {
        let first = which(reader.read());
        let second = which(reader.read());
        (first, second)
    })
}

#[test]
fn a_reader_never_sees_a_mixed_or_older_copy() {
    loom::model(|| {
        let mut writer = SeqWriter::<2>::new();
        let reader = writer.reader();
        let handle = reader_thread(reader.clone());
        for w in WRITES {
            writer.write([w, w]);
        }
        let (first, second) = handle.join().unwrap();
        // A `None` is a read that gave up, not an older write: only two
        // `Some`s are ordered.
        if first != 0 && second != 0 {
            assert!(second >= first, "a read went back from {first} to {second}");
        }
        assert_eq!(reader.read(), Some([2, 2]), "the last write is read");
    });
}
