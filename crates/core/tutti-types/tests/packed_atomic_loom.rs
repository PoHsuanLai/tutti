//! A `loom` model of [`PackedAtomic`]: the shipped code, whose atomic is
//! loom's under the flag (see `rt/packed_atomic.rs`'s `sync` module).
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p tutti-types --release --test packed_atomic_loom
//! ```
//!
//! The value is a range, `[min, max]` as two `f32`s. It asserts, over every
//! interleaving of one writer and one reader:
//!
//! - **no torn pair**: every load returns the `(min, max)` of the initial
//!   value or of one store, never one store's `min` with another's `max`;
//! - **no going back**: two loads in program order never see an older store
//!   after a newer one (the word is a single location, so coherence holds).
//!
//! # Mutation record
//!
//! Each was applied to `rt/packed_atomic.rs` and the model failed:
//!
//! - `store` writing the word's two halves separately (a `fetch_and`
//!   clearing one half, then a `fetch_or` of its new bytes, then the same
//!   for the other), as two `AtomicF32`s would → "a torn range";
//! - `load` reading the word twice and taking the low half from the first
//!   read and the high half from the second → "a torn range".

#![cfg(loom)]

use std::sync::Arc;

use loom::thread;
use tutti_types::PackedAtomic;

const STORES: [[f32; 2]; 2] = [[1.0, 2.0], [3.0, 4.0]];
const INITIAL: [f32; 2] = [-1.0, -2.0];

/// The index of `pair` in the history (0: the initial value), or a panic.
fn which(pair: [f32; 2]) -> usize {
    std::iter::once(INITIAL)
        .chain(STORES)
        .position(|p| p == pair)
        .unwrap_or_else(|| panic!("a torn range: {pair:?}"))
}

#[test]
fn a_reader_never_sees_a_torn_or_older_range() {
    loom::model(|| {
        let cell = Arc::new(PackedAtomic::new(INITIAL));
        let writer = {
            let cell = Arc::clone(&cell);
            thread::spawn(move || {
                for range in STORES {
                    cell.store(range);
                }
            })
        };
        let first = which(cell.load());
        let second = which(cell.load());
        assert!(second >= first, "a load went back from {first} to {second}");
        writer.join().unwrap();
        assert_eq!(cell.load(), STORES[1]);
    });
}
