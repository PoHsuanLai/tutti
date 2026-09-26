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
//! the real `claim`/`idle`/`push`/`retract_to`/`raise_from`/`reset` can take.
//! (tutti-sampler, which wraps the ring, cannot build under the flag:
//! `event-listener`, under its async channels, reacts to `cfg(loom)` without
//! depending on loom. That is why the protocol lives here, beside
//! `RtPublish`.)
//!
//! # What each model asserts
//!
//! A ring of 4 slots, 2 channels. Every sample the writer stores is a tag of
//! its position, the generation it was written under and its channel. A
//! reader thread makes two claims (one, in the models whose writer does
//! most) of two ranges each; for every position of a claim's ranges its
//! window holds, it reads channel 0, channel 1, then
//! channel 0 again, and asserts they are the tags of *that* position (not
//! another sharing its slot), of one frame (not torn across channels),
//! unchanged across the claim (not rewritten under it). The writer, on the
//! model's main thread:
//!
//! - `a_write_round_the_ring_never_lands_under_a_claim` — appends round the
//!   ring while the reader's second claim reads the slots the first did not;
//! - `a_second_range_is_kept` — appends round the ring onto the slot of a
//!   position only the reader's *second* range holds;
//! - `retract_then_push` — retracts under a claim, then rewrites past the
//!   retraction (the second review of #48, B3);
//! - `reset_then_push` — resets below the old end, then rewrites there;
//! - `retract_twice_then_push` — two shrinks before the rewrite (the stale
//!   span kept, or cleared, across them);
//! - `raise_then_reset_then_push` — a raise, then a reset under it;
//! - `reset_then_wrap` — a reset, then appends that go round the ring from
//!   the new place;
//! - `a_capped_push_then_reset_keeps_the_claim` — a push cut to nothing by
//!   the second range's alias, then a reset and a rewrite there (the review
//!   of `PosRing`, B1);
//! - `a_partly_capped_push_then_reset_keeps_the_claim` — the same with a push
//!   that lands one frame of three.
//!
//! # Mutations (run)
//!
//! - The generation check removed from `push` → `retract_then_push`,
//!   `reset_then_push` fail (B3, found before its fix, shown in PR #48).
//! - The start raise before the range load removed →
//!   `a_write_round_the_ring_never_lands_under_a_claim` fails.
//! - The reader loading the window before storing its ranges → the same
//!   fails.
//! - `shrink` bumping the generation before storing the window →
//!   `retract_then_push`, `reset_then_push` fail.
//! - Every fence removed → `a_write_round_the_ring_never_lands_under_a_claim`
//!   and `retract_then_push` fail.
//! - The shipped `push` (no first cap from the ranges, the raise kept when
//!   the write is cut short) → `a_capped_push_then_reset_keeps_the_claim`
//!   fails (B1, found before its fix, shown in PR #48).
//! - Only the restore of a write cut to nothing removed → the same fails.
//! - Only the final window's start the raise's (the first cap kept) →
//!   `a_partly_capped_push_then_reset_keeps_the_claim` fails: the ranges
//!   the first cap reads before the fence can be older than the ones the
//!   check reads after it.
//! - The second range never published (`reads[2..4]` left stale) →
//!   `a_second_range_is_kept` and `a_capped_push_then_reset_keeps_the_claim`
//!   fail.
//! - `raise_from` recording nothing stale → `raise_then_reset_then_push`
//!   fails.
//! - The stale span cleared at every shrink (not only once the reader has
//!   seen it) → `raise_then_reset_then_push` fails.
//!
//! **Survivor, documented (M6):** the echo `claimed` stored `Relaxed`
//! instead of `Release`. What the `Release` orders is the reader's earlier
//! *loads* (the block before) before the echo: without it, a load of that
//! block could read a write the writer made after seeing the echo — load
//! buffering. Loom does not model load buffering (a load never reads a store
//! that has not happened yet in its interleaving), so no model here can fail
//! on it; C++ allows it, and the `Release` is what rules it out, as in
//! `rt_publish_loom.rs`.
#![cfg(loom)]

use loom::thread;
use tutti_types::{PosReader, PosRing, PosWriter};

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

/// A ring holding positions `0..4` written under generation 0, keeping
/// `keep` behind the reader, which the writer says plays at `play`.
fn full_ring(keep: u64, play: u64) -> (PosWriter, PosReader) {
    let (mut writer, reader) = PosRing::new(4, STRIDE, keep);
    writer.set_play(4);
    assert_eq!(writer.push(&frames(0, 4, 0)), 4);
    writer.set_play(play);
    (writer, reader)
}

type Ranges = [(u64, u64); 2];

/// Claims `(play, ranges)` on a reader thread, checking every held position
/// (module docs).
fn read<const N: usize>(
    mut reader: PosReader,
    claims: [(u64, Ranges); N],
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for (play, ranges) in claims {
            let claim = reader.claim(play, ranges);
            for s in ranges.iter().flat_map(|&(a, e)| a..e) {
                if !claim.holds(s) {
                    continue;
                }
                let (x0, x1, y0) = (claim.sample(s, 0), claim.sample(s, 1), claim.sample(s, 0));
                assert_eq!((x0 as u64) / 100, s, "position {s} held another's frame");
                assert_eq!(x1, x0 + 1.0, "position {s} torn across channels");
                assert_eq!(y0, x0, "position {s} rewritten under the claim");
            }
        }
    })
}

const NONE: (u64, u64) = (0, 0);

#[test]
fn a_write_round_the_ring_never_lands_under_a_claim() {
    loom::model(|| {
        let (mut writer, reader) = full_ring(1, 1);
        let reading = read(reader, [(4, [(2, 4), NONE]), (4, [(0, 2), NONE])]);
        writer.set_play(4);
        writer.push(&frames(4, 3, 0));
        reading.join().expect("the reader");
    });
}

#[test]
fn a_second_range_is_kept() {
    loom::model(|| {
        let (mut writer, reader) = full_ring(0, 3);
        let reading = read(reader, [(3, [(3, 4), (0, 1)]), (3, [(3, 4), (0, 1)])]);
        writer.push(&frames(4, 2, 0));
        reading.join().expect("the reader");
    });
}

#[test]
fn retract_then_push() {
    loom::model(|| {
        let (mut writer, reader) = full_ring(1, 1);
        let reading = read(reader, [(1, [(0, 4), NONE]), (1, [(0, 4), NONE])]);
        writer.retract_to(2);
        writer.push(&frames(2, 2, 1));
        reading.join().expect("the reader");
    });
}

#[test]
fn reset_then_push() {
    loom::model(|| {
        let (mut writer, reader) = full_ring(1, 1);
        let reading = read(reader, [(1, [(0, 4), NONE]), (1, [(0, 4), NONE])]);
        writer.reset(1);
        writer.push(&frames(1, 3, 1));
        reading.join().expect("the reader");
    });
}

#[test]
fn retract_twice_then_push() {
    loom::model(|| {
        let (mut writer, reader) = full_ring(1, 1);
        let reading = read(reader, [(1, [(1, 3), NONE])]);
        writer.retract_to(3);
        writer.retract_to(1);
        writer.push(&frames(1, 2, 1));
        reading.join().expect("the reader");
    });
}

#[test]
fn raise_then_reset_then_push() {
    loom::model(|| {
        let (mut writer, reader) = full_ring(1, 1);
        let reading = read(reader, [(1, [(0, 2), NONE])]);
        writer.raise_from(2);
        writer.reset(0);
        writer.push(&frames(0, 1, 1));
        reading.join().expect("the reader");
    });
}

#[test]
fn reset_then_wrap() {
    loom::model(|| {
        let (mut writer, reader) = full_ring(0, 2);
        let reading = read(reader, [(2, [(1, 3), NONE]), (2, [(1, 3), NONE])]);
        writer.reset(2);
        // 2..6 over four slots: 4 and 5 go round onto 0 and 1.
        writer.push(&frames(2, 4, 1));
        reading.join().expect("the reader");
    });
}

#[test]
fn a_capped_push_then_reset_keeps_the_claim() {
    loom::model(|| {
        let (mut writer, reader) = full_ring(0, 2);
        let reading = read(reader, [(2, [(2, 3), (0, 1)])]);
        writer.push(&frames(4, 2, 0));
        writer.reset(0);
        writer.push(&frames(0, 1, 1));
        reading.join().expect("the reader");
    });
}

/// The write cut short but not to nothing: 4 lands (0's slot), 5 is 1's
/// alias. The window must keep 1..3 — a start left at 3 (the raise, from the
/// ranges read before the fence) would drop 1, the reset would not count it
/// stale, and the rewrite would land on it under the claim.
#[test]
fn a_partly_capped_push_then_reset_keeps_the_claim() {
    loom::model(|| {
        let (mut writer, reader) = full_ring(0, 3);
        let reading = read(reader, [(3, [(3, 4), (1, 2)])]);
        writer.push(&frames(4, 3, 0));
        writer.reset(0);
        writer.push(&frames(0, 3, 1));
        reading.join().expect("the reader");
    });
}
