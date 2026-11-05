//! A `loom` model of [`edit_ring`]: the shipped code, whose atomics are
//! loom's under the flag (see `rt/edit_ring.rs`'s `sync` module).
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p tutti-types --release --test edit_ring_loom
//! ```
//!
//! A ring of 2 slots holding `[u32; 3]` (twelve bytes: two words, the second
//! half used). The producer pushes the values 1, 2, 3, each element the
//! value's number, so a torn slot shows as unequal elements; a push the ring
//! refuses is retried after the consumer has popped, as the control thread's
//! next tick would. The consumer pops until it has seen all three. Over every
//! interleaving:
//!
//! - **applied once, in order**: the consumer sees 1, 2, 3, each once;
//! - **no torn slot**: every popped value has equal elements;
//! - **refusals are counted**: the producer's refusal count equals the
//!   pushes that returned `RingFull`.
//!
//! The ring is smaller than the stream, so the producer reuses a slot the
//! consumer is finishing with: the case a missing `Release` on `tail` breaks.
//!
//! # Mutation record
//!
//! Each was applied to `rt/edit_ring.rs`; the model failed as noted.
//!
//! - `push`'s store of `head` weakened to `Relaxed` → "out of order or
//!   repeated: 1 after 2" (the consumer sees the new `head` before the slot's
//!   words);
//! - `pop`'s load of `head` weakened to `Relaxed` → the same family of
//!   failure;
//! - `push` ignoring a full ring (no `head - tail` check) → "a torn slot:
//!   [1, 1, 3]" (a slot rewritten while the consumer reads it);
//! - `pop` not advancing `tail` → "out of order or repeated: 1 after 1".
//!
//! Not covered here: weakening `pop`'s store of `tail` to `Relaxed`, or
//! `push`'s load of it, **survives**. The `Release` on `tail` keeps the
//! consumer's slot reads from moving after the store that frees the slot, a
//! load-then-store reordering that loom does not explore. Those two orderings
//! are argued from the protocol in `rt/edit_ring.rs`, not shown by this model.

#![cfg(loom)]

use loom::thread;
use tutti_types::edit_ring;

#[test]
fn every_edit_is_applied_once_in_order() {
    loom::model(|| {
        let (mut writer, mut reader) = edit_ring::<[u32; 3]>(2);

        let producer = thread::spawn(move || {
            let mut refused = 0;
            for n in 1..=3u32 {
                // A refused push is retried once; the consumer may not have
                // popped yet, so the value can be dropped: the model keeps
                // only the order of what is applied.
                if writer.push([n; 3]).is_err() {
                    refused += 1;
                    thread::yield_now();
                    if writer.push([n; 3]).is_err() {
                        refused += 1;
                    }
                }
            }
            (refused, writer.refused())
        });

        let mut last = 0;
        for _ in 0..6 {
            if let Some([a, b, c]) = reader.pop() {
                assert!(a == b && b == c, "a torn slot: {:?}", [a, b, c]);
                assert!(
                    a > last && a <= 3,
                    "out of order or repeated: {a} after {last}"
                );
                last = a;
            } else {
                thread::yield_now();
            }
        }

        let (refused, counted) = producer.join().unwrap();
        assert_eq!(refused, counted, "refusals miscounted");
        while let Some([a, b, c]) = reader.pop() {
            assert!(a == b && b == c, "a torn slot");
            assert!(
                a > last && a <= 3,
                "out of order or repeated: {a} after {last}"
            );
            last = a;
        }
        assert!(last >= 1, "nothing was applied");
    });
}
