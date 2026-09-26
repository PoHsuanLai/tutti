use super::*;

fn indexed(frames: usize, channels: usize) -> Vec<f32> {
    (0..frames * channels).map(|i| i as f32).collect()
}

/// A ring of `frames` slots of `stride`, keeping `keep` behind.
fn ring(frames: usize, stride: usize, keep: u64) -> (PosWriter, PosReader) {
    PosRing::new(frames, stride, keep)
}

/// Position `s` through a claim of exactly its frame.
fn frame_at(reader: &mut PosReader, s: u64) -> Vec<f32> {
    let stride = reader.stride();
    let claim = reader.claim(s, [(s, s + 1), (0, 0)]);
    (0..stride).map(|c| claim.sample(s, c)).collect()
}

/// **The window packs into one word and back**, up to its largest length
/// and far along the line.
///
/// Mutation (run): the length mask one bit short → the longest window
/// unpacks short → fails.
#[test]
fn a_window_packs_into_one_word_and_back() {
    let far = 1u64 << 39;
    for (from, to) in [(0, 0), (5, 4_100), (far, far + MAX_POS_RING_FRAMES as u64)] {
        let w = RingWindow { from, to };
        assert_eq!(RingWindow::unpack(w.pack()), w);
    }
}

/// **Frames land at the window's end and read back by position**, every
/// channel in its place at six channels, a trailing partial frame
/// dropped.
///
/// Mutation (run): the slot index taken in samples (`slot * ch` → `slot`)
/// → channels rotate → fails.
#[test]
fn frames_land_at_the_windows_end_and_read_back_by_position() {
    let (mut writer, mut reader) = ring(64, 6, 4);
    let mut data = indexed(10, 6);
    data.extend_from_slice(&[99.0, 99.0, 99.0]);
    assert_eq!(writer.push(&data), 10);
    assert_eq!(writer.window(), RingWindow { from: 0, to: 10 });
    for s in 0..10 {
        let want: Vec<f32> = (0..6).map(|c| (s as usize * 6 + c) as f32).collect();
        assert_eq!(frame_at(&mut reader, s), want, "frame {s}");
    }
}

/// **A write never goes round the ring onto the reader**: with the reader
/// at 1 000, a ring of 4 096 that keeps 4 behind takes positions up to
/// 1 000 - 4 + 4 096 and no further.
///
/// Mutation (run): the room check removed → the push wraps onto 996.. →
/// fails.
#[test]
fn a_write_never_goes_round_the_ring_onto_the_reader() {
    let (mut writer, mut reader) = ring(4_096, 1, 4);
    writer.push(&indexed(1_000, 1));
    writer.set_play(1_000);
    assert_eq!(writer.push(&indexed(8_000, 1)), 4_096 - 4);
    assert_eq!(writer.window().to, 1_000 - 4 + 4_096);
    assert_eq!(frame_at(&mut reader, 996), [996.0]);
}

/// **A write never reuses a slot the block in flight reads**: a block
/// reading `[3 997, 4 069)` whose jump copy reads `[100, 200)` keeps both
/// — the writer stops at 4 196, the first position whose slot is 100's —
/// while it writes the block's own positions freely. And the window still
/// holds 100..: the write took out only what it overwrote (the review of
/// `PosRing`, B1: the start was raised to 204 for the 300 frames asked for,
/// and left there when the write stopped at 196, so the block's jump copy
/// was lost to the window).
///
/// Mutation (run): the alias check removed → 4 196.. overwrite 100.. →
/// fails. Mutation (run): a position *in* the range counted as its own
/// alias → the writer stops at once → fails. Mutation (run): the final
/// window's start the raise's, with no first cap from the ranges (the
/// shipped code) → the window no longer holds 150 → fails.
#[test]
fn a_write_never_reuses_a_slot_the_block_in_flight_reads() {
    let (mut writer, mut reader) = ring(4_096, 1, 4);
    writer.push(&indexed(4_000, 1));
    let claim = reader.claim(3_997, [(3_997, 4_069), (100, 200)]);
    assert_eq!(claim.window(), RingWindow { from: 0, to: 4_000 });
    assert_eq!(writer.push(&[-1.0; 300]), 196, "stops at 4 196");
    assert_eq!(claim.sample(150, 0), 150.0);
    assert!(writer.window().holds(150), "the jump copy left the window");
}

/// **A write cut short takes out of the window only what it overwrote**
/// (the review of `PosRing`, B1). A block claims `[10, 14)` and `[2, 5)` of
/// a full ring of 16; a push of 8 is cut to 2 by 2's alias at 18, so the
/// window must still hold 2.. — and a reset then counts 2..5 as stale, so
/// the rewrite at 2 waits for the reader. The shipped code raised the start
/// to 8 for the 8 asked for and kept it, so the reset did not count 2..5
/// and the rewrite landed under the claim.
///
/// Mutation (run): the shipped code (no first cap from the ranges, the
/// final window's start the raise's) → the window is left at `[8, 18)` →
/// fails (past that assertion, the review's repro: `sample(3)` is -9). (Single-threaded, the first cap from the
/// ranges alone also keeps it: removing *only* the final start's fix is
/// caught by the loom model
/// `a_partly_capped_push_then_reset_keeps_the_claim`.)
#[test]
fn a_capped_write_keeps_what_it_did_not_overwrite() {
    for retract in [false, true] {
        let (mut writer, mut reader) = ring(16, 1, 0);
        writer.set_play(10);
        assert_eq!(writer.push(&indexed(16, 1)), 16);
        let claim = reader.claim(10, [(10, 14), (2, 5)]);
        assert_eq!(writer.push(&[-1.0; 8]), 2);
        assert_eq!(writer.window(), RingWindow { from: 2, to: 18 });
        if retract {
            writer.retract_to(2);
        } else {
            writer.reset(2);
        }
        writer.push(&[-9.0; 3]);
        assert_eq!(claim.sample(3, 0), 3.0, "retract {retract}: rewritten");
    }
}

/// **A shrink does not free what a block in flight holds** (the second
/// review of #48, B3): a block claims `[600, 680)` in the window `[0,
/// 1 000)`; the writer retracts to 620 and pushes a rewrite — it must not
/// touch 620.. until the reader claims again, after which it may. The
/// same for a reset below the old end.
///
/// Mutation (run): the generation check removed from `push` → frame 650
/// changes under the claim → fails. And a reset that moves the window
/// away from a claim frees the new place at once: what it removed is the
/// old window, not everything below its end (a first cut blocked a seek's
/// refill until the reader claimed again, and the refill reset the window
/// again every cycle). Mutation (run): `shrink` bumping the
/// generation *before* storing the window → no single-threaded change
/// (the loom model `retract_then_push` catches the order).
#[test]
fn a_shrink_does_not_free_what_a_block_in_flight_holds() {
    for reset in [false, true] {
        let (mut writer, mut reader) = ring(4_096, 1, 4);
        writer.push(&indexed(1_000, 1));
        {
            let claim = reader.claim(600, [(600, 680), (0, 0)]);
            assert!(claim.holds(650));
            if reset {
                writer.reset(620);
            } else {
                writer.retract_to(620);
            }
            assert_eq!(
                writer.push(&[-1.0; 100]),
                0,
                "reset {reset}: wrote under the claim"
            );
            assert_eq!(claim.sample(650, 0), 650.0, "reset {reset}");
        }
        // The next claim sees the shrink: the rewrite may land.
        let claim = reader.claim(600, [(600, 680), (0, 0)]);
        assert!(!claim.holds(650));
        assert_eq!(writer.push(&[-1.0; 100]), 100, "reset {reset}");
    }
    // A reset away from a claim (a seek back, below the old window) frees
    // the new place at once: the old window was `[5 000, 6 000)`.
    let (mut writer, mut reader) = ring(4_096, 1, 4);
    writer.reset(5_000);
    writer.set_play(5_000);
    writer.push(&indexed(1_000, 1));
    let _claim = reader.claim(1_000, [(997, 1_069), (0, 0)]);
    writer.reset(996);
    writer.set_play(1_000);
    assert_eq!(writer.push(&[-1.0; 100]), 100, "a seek's refill was held");
}

/// **Raising the window's start drops the positions below it**, so a
/// reader that jumps back there finds the window without them — and once
/// the reader has claimed since, a reset there refills at once: what the
/// raise removed is no longer counted as held.
///
/// Mutation (run): the stale span not cleared at a shrink the reader has
/// seen → the reset's refill is held for a cycle → fails.
#[test]
fn raising_the_start_drops_what_lies_below() {
    let (mut writer, mut reader) = ring(4_096, 1, 4);
    writer.push(&indexed(1_000, 1));
    writer.raise_from(400);
    assert_eq!(
        writer.window(),
        RingWindow {
            from: 400,
            to: 1_000
        }
    );
    assert_eq!(writer.generation(), 1);
    let _ = reader.claim(500, [(497, 569), (0, 0)]);
    // The reader jumps back below the raise; the writer moves the window.
    let _claim = reader.claim(100, [(97, 169), (0, 0)]);
    writer.reset(96);
    writer.set_play(100);
    assert_eq!(writer.push(&[-1.0; 100]), 100, "the refill was held");
}

/// **A reader that stops claiming holds nothing back** (the review of
/// `PosRing`, S2): a block claims `[97, 169)`, the reader goes idle, the
/// writer moves the window to where `97`'s slot comes round again and
/// fills — at once, not after a claim that never comes.
///
/// Mutation (run): `idle` leaving the ranges → the fill stops at the
/// alias of 97 → fails. Not covered, and why: `idle` also echoes the
/// generation, which only lets the writer forget stale spans at its next
/// shrink rather than at the reader's next claim. With the ranges cleared,
/// a stale span holds nothing back (it counts only where it meets a range),
/// and the next claim echoes anyway, so no single-threaded order — and no
/// loom model here — can tell the echo is missing; it is kept because the
/// reader has, in fact, seen the generation.
#[test]
fn an_idle_reader_holds_nothing_back() {
    let (mut writer, mut reader) = ring(1_024, 1, 4);
    writer.push(&indexed(1_000, 1));
    let _ = reader.claim(100, [(97, 169), (0, 0)]);
    reader.idle();
    writer.reset(97 + 1_024);
    writer.set_play(97 + 1_024 + 4);
    assert_eq!(writer.push(&[-1.0; 500]), 500, "held by an idle reader");
    assert_eq!(writer.in_flight_end(), 0);
}

/// `first_alias` against a brute-force search, over ranges before, around
/// and after the write's start, near and across a lap of the ring — up to
/// twice the ring's length (the review of `PosRing`, N5).
///
/// Mutation (run): `a + n` returned as `a` for a range containing the
/// start → fails. Mutation (run): the start inside a range longer than the
/// ring answered `a + n` (the code before N5) → fails.
#[test]
fn first_alias_is_the_first_slot_another_position_of_the_range_holds() {
    let n = 16u64;
    for to in 0..64u64 {
        for a in 0..64u64 {
            for len in 0..=2 * n {
                let e = a + len;
                let brute = (to..to + 4 * n)
                    .find(|&s| (a..e).any(|q| q != s && q % n == s % n))
                    .unwrap_or(u64::MAX);
                let got = first_alias(to, a, e, n);
                assert!(
                    got == brute || (brute == u64::MAX && got >= to + 4 * n),
                    "to {to} range [{a}, {e}): {got}, brute {brute}"
                );
            }
        }
    }
}

/// **A writer thread and a reader thread, free-running**: the writer
/// pushes, retracts, raises and resets at random; the reader claims random
/// ranges (two, the second often far behind) or idles, and checks every
/// sample it reads in its window is the tag of that position (both
/// channels, twice), so no write ever lands under a claim. Real threads, not
/// a model: it complements `tests/pos_ring_loom.rs` at a scale loom cannot
/// explore.
///
/// Mutation (run): the generation check removed from `push` → a torn or
/// rewritten frame under a claim within the run → fails.
#[test]
fn a_writer_and_a_reader_thread_never_tear() {
    use std::sync::atomic::{AtomicBool, Ordering as O};
    const N: usize = 64;
    let (mut writer, mut reader) = ring(N, 2, 2);
    let watch = writer.watch();
    let done = std::sync::Arc::new(AtomicBool::new(false));
    // A tag: the position, the rewrite generation, the channel — kept
    // under 2^24, where an `f32` holds every integer exactly.
    let tag =
        |pos: u64, gen: u64, c: usize| ((pos % 8_192) * 128 + (gen % 32) * 2 + c as u64) as f32;
    let reading = {
        let done = std::sync::Arc::clone(&done);
        std::thread::spawn(move || {
            let mut seed = 0x9e37_79b9_u64;
            let mut checked = 0u64;
            while !done.load(O::Relaxed) {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                if seed.is_multiple_of(64) {
                    reader.idle();
                    continue;
                }
                let w = watch.window();
                let a = w.from + seed % (w.to - w.from + 1).max(1);
                let e = a + 1 + (seed >> 20) % 8;
                let b = a.saturating_sub((seed >> 32) % (N as u64));
                let f = b + 1 + (seed >> 40) % 4;
                let claim = reader.claim(a, [(a, e), (b, f)]);
                for s in (a..e).chain(b..f) {
                    if !claim.holds(s) {
                        continue;
                    }
                    let (x0, x1, y0) = (claim.sample(s, 0), claim.sample(s, 1), claim.sample(s, 0));
                    let pos = (x0 as u64) / 128;
                    assert_eq!(pos, s % 8_192, "position {s} held another's frame");
                    assert_eq!(x1, x0 + 1.0, "position {s} torn across channels");
                    assert_eq!(y0, x0, "position {s} rewritten under the claim");
                    checked += 1;
                }
            }
            checked
        })
    };
    let mut seed = 0x2545_f491_u64;
    let mut gen = 0u64;
    let mut buf = Vec::new();
    for _ in 0..200_000 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let w = writer.window();
        match seed % 16 {
            0 => {
                gen += 1;
                writer.retract_to(w.from + (seed >> 8) % (w.to - w.from + 1));
            }
            1 => {
                gen += 1;
                let play = writer.play();
                writer.reset(play.saturating_sub(2) + (seed >> 8) % 8);
            }
            2 => {
                gen += 1;
                writer.raise_from(w.from + (seed >> 8) % (w.to - w.from + 1));
            }
            _ => {
                let to = writer.window().to;
                let n = 1 + (seed >> 8) as usize % 16;
                buf.clear();
                for i in 0..n as u64 {
                    buf.push(tag(to + i, gen, 0));
                    buf.push(tag(to + i, gen, 1));
                }
                writer.push(&buf);
            }
        }
    }
    done.store(true, O::Relaxed);
    let checked = reading.join().expect("the reader never saw a torn frame");
    assert!(checked > 1_000, "the reader checked only {checked} frames");
}
