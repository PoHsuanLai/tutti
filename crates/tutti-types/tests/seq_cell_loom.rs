//! A `loom` model of [`SeqWriter`] and [`SeqReader`]: the shipped code, whose atomics and fences
//! are loom's under the flag (see `rt/seq_cell.rs`'s `sync` module).
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p tutti-types --release --test seq_cell_loom
//! ```
//!
//! The value is a `[u32; 3]`: twelve bytes, so two words, the second only
//! half used, and the model covers the padding too. Each write stores one
//! number in every element, so a copy mixing two writes shows as unequal
//! elements. It asserts, over every interleaving of one writer and one
//! reader, with the writer on either thread:
//!
//! - **no mixed copy**: a read returns "never written", `Contended`, or the
//!   value of one write, never bytes of two;
//! - **no going back**: two reads in program order never return an older
//!   write after a newer one, nor "never written" after a write;
//! - **the last write is read**, on the first copy, once the writer is done.
//!
//! # Mutation record
//!
//! Each was applied to `rt/seq_cell.rs` (re-run against the typed
//! `SeqWriter<T>`) and both models failed, at `LOOM_MAX_PREEMPTIONS=3` and
//! unbounded (the pair runs in about four seconds):
//!
//! - the writer's `Release` fence deleted → "a mixed copy: [1, 1, 2]" (the
//!   first word from one write, the second from the other);
//! - the reader's `Acquire` fence deleted → the same;
//! - the reader's re-check of the sequence deleted (the copy returned as
//!   loaded) → the same;
//! - the reader not refusing an odd sequence (copying mid-write) → "a mixed
//!   copy: [0, 0, 0]" (bytes no write stored);
//! - the writer's closing store weakened to `Relaxed` → the same;
//! - the reader's first sequence load weakened to `Relaxed` → the same;
//! - the reader returning "never written" for an odd sequence → "\"never
//!   written\" after Write(1)".
//!
//! Not covered here: `Contended` reported as "never written" when the
//! retries run out passes, because one remaining write cannot outlast the
//! retries of a read that already saw the first. The unit test
//! `a_read_tells_never_written_from_contended` pins that, on a cell held
//! mid-write.

#![cfg(loom)]

use loom::thread;
use tutti_types::{Contended, SeqReader, SeqWriter};

type Value = [u32; 3];

/// The writes, in order. Elements are the write's index, so 0 is "never
/// written".
const WRITES: [u32; 2] = [1, 2];

/// The retries a read is allowed: as many copies as the VST2 host allows.
const RETRIES: usize = 7;

/// What a read saw.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Seen {
    Never,
    GaveUp,
    Write(u32),
}

/// What a read returned, or a panic on a mixed copy.
fn which(read: Result<Option<Value>, Contended>) -> Seen {
    match read {
        Ok(None) => Seen::Never,
        Err(Contended) => Seen::GaveUp,
        Ok(Some([a, b, c])) if a == b && b == c && WRITES.contains(&a) => Seen::Write(a),
        // Unequal elements, or elements no write stored.
        Ok(Some(value)) => panic!("a mixed copy: {value:?}"),
    }
}

/// Two reads in program order: the second never older than the first, and
/// never "never written" after a write.
fn two_reads(reader: &SeqReader<Value>) {
    let first = which(reader.read(RETRIES));
    let second = which(reader.read(RETRIES));
    match (first, second) {
        (Seen::Write(a), Seen::Write(b)) => {
            assert!(b >= a, "a read went back from {a} to {b}");
        }
        (Seen::Write(_) | Seen::GaveUp, Seen::Never) => {
            panic!("\"never written\" after {first:?}");
        }
        // A `Contended` is a read that gave up, not an older write.
        _ => {}
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
// `arc-lend`'s `two_publishers_and_a_reader` and several `pos_ring` models
// by 4 to 24%; that fix is a separate change.)

/// The writer on the main thread, the reader spawned.
#[test]
fn a_spawned_reader_never_sees_a_mixed_or_older_copy() {
    loom::model(|| {
        let mut writer = SeqWriter::<Value>::new();
        let reader = writer.reader();
        let handle = {
            let reader = reader.clone();
            thread::spawn(move || two_reads(&reader))
        };
        for w in WRITES {
            writer.write([w, w, w]);
        }
        handle.join().unwrap();
        assert_eq!(reader.read(0), Ok(Some([2; 3])), "the last write is read");
    });
}

/// The writer spawned, the reader on the main thread.
#[test]
fn a_reader_beside_a_spawned_writer_never_sees_a_mixed_or_older_copy() {
    loom::model(|| {
        let mut writer = SeqWriter::<Value>::new();
        let reader = writer.reader();
        let handle = thread::spawn(move || {
            for w in WRITES {
                writer.write([w, w, w]);
            }
        });
        two_reads(&reader);
        handle.join().unwrap();
        assert_eq!(reader.read(0), Ok(Some([2; 3])), "the last write is read");
    });
}
