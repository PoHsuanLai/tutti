//! A `loom` model of [`SeqWriter`] and [`SeqReader`]: the shipped code, whose atomics and fences
//! are loom's under the flag (see `rt/seq_cell.rs`'s `sync` module).
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p tutti-types --release --test seq_cell_loom
//! ```
//!
//! Each write stores one value in every word, so a copy mixing two writes
//! shows as unequal words. It asserts, over every interleaving of one writer
//! and one reader, with the writer on either thread:
//!
//! - **no mixed copy**: a read returns `None` or the words of one write,
//!   never words of two;
//! - **no going back**: two reads in program order never return an older
//!   write after a newer one;
//! - **the last write is read** once the writer is done.
//!
//! # Mutation record
//!
//! Each was applied to `rt/seq_cell.rs` (re-run against `SeqWriter`) and
//! both models failed, at `LOOM_MAX_PREEMPTIONS=3` and unbounded (the pair
//! runs in about three and a half seconds):
//!
//! - the writer's `Release` fence deleted → "a mixed copy: [1, 2]";
//! - the reader's `Acquire` fence deleted → "a mixed copy: [1, 2]";
//! - the reader's re-check of the sequence deleted (the copy returned as
//!   loaded) → "a mixed copy: [1, 2]";
//! - the reader not refusing an odd sequence (copying mid-write) → "a mixed
//!   copy: [0, 0]" (words no write stored);
//! - the writer's closing store weakened to `Relaxed` → the same (with the
//!   writer spawned, "a mixed copy: [1, 2]");
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

/// Two reads in program order: each a write or none, the second never older.
fn two_reads(reader: &SeqReader<2>) {
    let first = which(reader.read());
    let second = which(reader.read());
    // A `None` is a read that gave up, not an older write: only two `Some`s
    // are ordered.
    if first != 0 && second != 0 {
        assert!(second >= first, "a read went back from {first} to {second}");
    }
}

// Both placements of the writer are modelled. The first writer loaded the
// sequence before storing it, and loom 0.7 keeps one `last_access` per
// atomic: a thread that loads an atomic and then stores to it (or RMWs it)
// makes its own load the last access, so another thread's earlier load of
// that atomic drops out of the race check and loom never schedules the other
// order. A spawned thread doing `load` then `store`, against a main thread
// doing one `load`, runs one execution; with the roles swapped, six. With
// that writer spawned, this model ran once and every mutation below passed.
// The writer now only stores (it keeps its own sequence), so either
// placement explores; both run, so a writer that loads again cannot hide
// behind one of them. (The same limit under-explores `job_gate`,
// `rt_publish`'s `two_publishers_and_a_reader` and several `pos_ring` models
// by 4 to 24%; that fix is a separate change.)

/// The writer on the main thread, the reader spawned.
#[test]
fn a_spawned_reader_never_sees_a_mixed_or_older_copy() {
    loom::model(|| {
        let mut writer = SeqWriter::<2>::new();
        let reader = writer.reader();
        let handle = {
            let reader = reader.clone();
            thread::spawn(move || two_reads(&reader))
        };
        for w in WRITES {
            writer.write([w, w]);
        }
        handle.join().unwrap();
        assert_eq!(reader.read(), Some([2, 2]), "the last write is read");
    });
}

/// The writer spawned, the reader on the main thread.
#[test]
fn a_reader_beside_a_spawned_writer_never_sees_a_mixed_or_older_copy() {
    loom::model(|| {
        let mut writer = SeqWriter::<2>::new();
        let reader = writer.reader();
        let handle = thread::spawn(move || {
            for w in WRITES {
                writer.write([w, w]);
            }
        });
        two_reads(&reader);
        handle.join().unwrap();
        assert_eq!(reader.read(), Some([2, 2]), "the last write is read");
    });
}
