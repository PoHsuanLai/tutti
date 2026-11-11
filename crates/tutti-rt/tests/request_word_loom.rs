//! A `loom` model of [`RequestWord`]: the shipped code, whose atomics are
//! loom's under the flag (see `src/request_word.rs`'s `sync` module).
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p tutti-rt --release --test request_word_loom
//! ```
//!
//! Two threads raise one bit each while the main thread takes twice. Each
//! raiser also writes a plain-looking datum (a `Release` store to a second
//! atomic) before raising. Over every interleaving it asserts:
//!
//! - **no bit lost**: after both raisers join and a final take, every raised
//!   bit was returned by some take;
//! - **each bit taken once**: no bit appears in two takes, because a raise
//!   happens once and a take clears;
//! - **raise orders what came before it**: a taker that sees a bit also sees
//!   the datum its raiser wrote before raising.

#![cfg(loom)]

use loom::sync::Arc;
use loom::sync::atomic::{AtomicU32, Ordering};
use loom::thread;
use tutti_rt::{RequestBit, RequestWord};

const A: RequestBit = RequestBit::of(0);
const B: RequestBit = RequestBit::of(5);

#[test]
fn raises_from_two_threads_are_all_taken_once() {
    loom::model(|| {
        let word = Arc::new(RequestWord::new());
        let data = Arc::new([AtomicU32::new(0), AtomicU32::new(0)]);
        let raisers: Vec<_> = [(A, 0), (B, 1)]
            .into_iter()
            .map(|(bit, slot)| {
                let word = word.clone();
                let data = data.clone();
                thread::spawn(move || {
                    data[slot].store(7, Ordering::Relaxed);
                    word.raise(bit);
                })
            })
            .collect();

        let mut seen = [0u8; 2];
        for _ in 0..2 {
            let taken = word.take();
            for (n, (bit, slot)) in [(A, 0), (B, 1)].into_iter().enumerate() {
                if taken.contains(bit) {
                    seen[n] += 1;
                    assert_eq!(
                        data[slot].load(Ordering::Relaxed),
                        7,
                        "saw the bit without the datum"
                    );
                }
            }
        }
        for raiser in raisers {
            raiser.join().unwrap();
        }
        let last = word.take();
        for (n, bit) in [A, B].into_iter().enumerate() {
            if last.contains(bit) {
                seen[n] += 1;
            }
        }
        assert_eq!(seen, [1, 1], "a bit lost, or taken twice");
    });
}
