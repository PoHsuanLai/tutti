//! A `loom` model of `PosRing`'s no-tear protocol — the shipped code, not a
//! replica.
//!
//! ```text
//! LOOM_MAX_PREEMPTIONS=4 RUSTFLAGS="--cfg loom" \
//!     cargo test -p tutti-types --release --test pos_ring_loom
//! ```
//!
//! Under `--cfg loom`, `rt/pos_ring.rs` builds its atomics from `loom` (its
//! `sync` module), so every interleaving and reordering loom explores is one
//! the real `claim`/`push`/`retract_to`/`reset` can take. (tutti-sampler, which
//! wraps the ring, cannot build under the flag: `event-listener`, under its
//! async channels, reacts to `cfg(loom)` without depending on loom. That is
//! why the protocol lives here, beside `RtPublish`.)
//!
//! # What each model asserts
//!
//! A ring of 4 slots, 2 channels. Every sample the writer stores is a tag of
//! its position, the generation it was written under and its channel. A
//! reader thread makes two claims; for every position of a claim's range the
//! window holds, it reads channel 0, channel 1, then channel 0 again, and
//! asserts they are the tags of *that* position (not another sharing its
//! slot), of one frame (not torn across channels), unchanged across the claim
//! (not rewritten under it). The writer, on the model's main thread:
//!
//! - `a_write_round_the_ring_never_lands_under_a_claim` — appends round the
//!   ring while the reader's second claim reads the slots the first did not
//!   (the start raise before the range load, and the reader's range store
//!   before its window load, are both needed);
//! - `retract_then_push` — retracts under a claim, then rewrites past the
//!   retraction (the second review of #48, B3);
//! - `reset_then_push` — resets below the old end, then rewrites there.
//!
//! Mutations (run): the generation check removed from `push` →
//! `retract_then_push` and `reset_then_push` fail (B3, found before the fix
//! and shown in PR #48); the start raise before the range load removed →
//! `a_write_round_the_ring_never_lands_under_a_claim` fails; the reader
//! loading the window before storing its ranges → the same fails.
#![cfg(loom)]

use loom::thread;
use std::sync::Arc;
use tutti_types::PosRing;

const STRIDE: usize = 2;

fn tag(pos: u64, gen: u64, c: usize) -> f32 {
    (pos * 100 + gen * 10 + c as u64) as f32
}

/// `n` frames from position `from`, written under `gen`.
fn frames(from: u64, n: u64, gen: u64) -> Vec<f32> {
    (from..from + n)
        .flat_map(|p| (0..STRIDE).map(move |c| tag(p, gen, c)))
        .collect()
}

/// A ring holding positions `0..4`, written under generation 0.
fn full_ring() -> Arc<PosRing> {
    let ring = Arc::new(PosRing::new(4, STRIDE, 1));
    ring.set_play(1);
    assert_eq!(ring.push(&frames(0, 4, 0)), 4);
    ring
}

/// Two claims `(play, [a, e))`, checking every held position (module docs).
fn read(ring: &PosRing, claims: [(u64, (u64, u64)); 2]) {
    for (play, (a, e)) in claims {
        let window = ring.claim(play, [(a, e), (0, 0)]);
        for s in a..e {
            if !window.holds(s) {
                continue;
            }
            let (x0, x1, y0) = (ring.sample(s, 0), ring.sample(s, 1), ring.sample(s, 0));
            assert_eq!((x0 as u64) / 100, s, "position {s} held another's frame");
            assert_eq!(x1, x0 + 1.0, "position {s} torn across channels");
            assert_eq!(y0, x0, "position {s} rewritten under the claim");
        }
    }
}

#[test]
fn a_write_round_the_ring_never_lands_under_a_claim() {
    loom::model(|| {
        let ring = full_ring();
        let reader = {
            let ring = Arc::clone(&ring);
            thread::spawn(move || read(&ring, [(4, (2, 4)), (4, (0, 2))]))
        };
        ring.set_play(4);
        ring.push(&frames(4, 3, 0));
        reader.join().expect("the reader");
    });
}

#[test]
fn retract_then_push() {
    loom::model(|| {
        let ring = full_ring();
        let reader = {
            let ring = Arc::clone(&ring);
            thread::spawn(move || read(&ring, [(1, (0, 4)), (1, (0, 4))]))
        };
        ring.retract_to(2);
        ring.push(&frames(2, 2, 1));
        reader.join().expect("the reader");
    });
}

#[test]
fn reset_then_push() {
    loom::model(|| {
        let ring = full_ring();
        let reader = {
            let ring = Arc::clone(&ring);
            thread::spawn(move || read(&ring, [(1, (0, 4)), (1, (0, 4))]))
        };
        ring.reset(1);
        ring.push(&frames(1, 3, 1));
        reader.join().expect("the reader");
    });
}
