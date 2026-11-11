//! Ring refill for the streamer thread: keep each ring filled ahead of where
//! its reader plays.
//!
//! The reader publishes the straight position it plays (`Ring::play`); the
//! refill keeps the ring's window `[from, to)` running from just behind it to
//! most of the ring ahead of it, in the stream's mapping (`loops`). A reader
//! that has jumped outside the window — a seek, a varispeed change, a
//! transport loop — moves the window: it is emptied at the reader's
//! position and filled from there. Nothing is flushed; frames already written
//! stay valid for their positions, so a jump back inside the window costs
//! nothing.
//!
//! Every count here is denominated in frames. The `* ch` at each scratch
//! resize is the only place the interleave stride enters.

use super::super::cells::Cells;
#[cfg(test)]
use super::super::command::StreamId;
use super::super::loops::{Content, RingMap};
use super::super::metrics::Metrics;
use super::super::prefetch::{HISTORY_FRAMES, RegionOut};
use super::super::record::Record;
use rayon::prelude::*;
use std::sync::Arc;
use tutti_rt::MAX_POS_RING_POSITION;

/// The fraction of a ring ahead of its reader below which the streamer refills
/// it, and at which a stream counts as primed and healthy.
pub(in crate::streamer) const FILL_THRESHOLD: f32 = 0.75;

/// How many frames to read this cycle, under the varifill strategy.
///
/// Scales `base_chunk` by three independent factors, then clamps their product
/// to `0.25..=4.0` and floors the result at 1024 frames:
///
/// - Urgency — `1.0 - buffer_fill`, so an empty ring pulls harder.
/// - Bandwidth — the square root of `read_rate_bytes_per_sec` against a
///   10 MB/s baseline, clamped to `0.5..=2.0`. A non-positive or unmeasured rate
///   contributes a neutral 1.0.
/// - Speed — `playback_speed`, floored at 1.0: playing faster consumes the
///   ring faster, but playing slower is no reason to read in smaller pieces.
///
/// Robust to a nonsensical `buffer_fill` (negative, above one, or NaN) because
/// the clamp and the floor between them bound every path.
#[inline]
fn varifill_chunk(
    buffer_fill: f32,
    base_chunk: usize,
    read_rate_bytes_per_sec: f64,
    playback_speed: f32,
) -> usize {
    let urgency = (1.0 - buffer_fill) as f64;

    const BASELINE_RATE: f64 = 10_000_000.0;
    let bandwidth_factor = if read_rate_bytes_per_sec > 0.0 {
        (read_rate_bytes_per_sec / BASELINE_RATE)
            .sqrt()
            .clamp(0.5, 2.0)
    } else {
        1.0
    };

    let speed_factor = playback_speed.max(1.0) as f64;

    let multiplier = (0.5 + urgency * 1.5) * bandwidth_factor * speed_factor;

    let clamped = multiplier.clamp(0.25, 4.0);

    let chunk_size = (base_chunk as f64 * clamped) as usize;

    chunk_size.max(1024)
}

/// Refill every live stream's ring in turn, serially, each under its own
/// containment ([`Record::contained`]): a stream whose refill panics is
/// faulted and the rest are refilled.
///
/// `interleave_buffer` is the streamer's reusable scratch, passed in so a refill
/// does not allocate per cycle. Streamer thread throughout — this both blocks on
/// disk and may grow that buffer, so it must never run on the audio thread.
pub(crate) fn refill_all(
    records: &mut [Record],
    metrics: &Metrics,
    base_chunk_size: usize,
    interleave_buffer: &mut Vec<f32>,
) {
    let read_rate = metrics.read_rate();
    for record in records.iter_mut().filter(|r| r.is_live()) {
        refill_contained(record, base_chunk_size, read_rate, interleave_buffer);
    }
}

/// [`refill_one`] on `record` under its containment, then its status
/// published.
fn refill_contained(
    record: &mut Record,
    base_chunk_size: usize,
    read_rate: f64,
    buffer: &mut Vec<f32>,
) {
    let refilled =
        record.contained(|r| refill_one(&mut r.out, &r.cells, base_chunk_size, read_rate, buffer));
    if let Some(refilled) = refilled {
        record.settle(&refilled);
    }
}

/// The same refill across rayon workers, chosen when `parallel_io` is on and
/// three or more streams are open.
///
/// `par_iter_mut` over the records hands each worker an exclusive `&mut` to a
/// *different* one, whose mapping — loop included — travels with its writer, so
/// a looped stream refills here exactly as it does serially. Scratch is a
/// thread-local per worker.
pub(crate) fn refill_all_parallel(
    records: &mut [Record],
    metrics: &Metrics,
    base_chunk_size: usize,
) {
    let read_rate = metrics.read_rate();
    records
        .par_iter_mut()
        .filter(|r| r.is_live())
        .for_each(|record| {
            thread_local! {
                static LOCAL_BUF: std::cell::RefCell<Vec<f32>> =
                    std::cell::RefCell::new(Vec::with_capacity(16384));
            }
            LOCAL_BUF.with(|buf| {
                refill_contained(record, base_chunk_size, read_rate, &mut buf.borrow_mut());
            });
        });
}

/// What a ring's window says about where its reader plays: the one decision the
/// streamer's refill and a voice's ring both make (doc 025 §7), so they never
/// disagree about "below threshold".
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Refill {
    /// The reader is outside the window, behind it or far past its end: the
    /// window moves to the reader and fills from there.
    Jumped,
    /// The window is at its refill threshold ahead of the reader, or holds
    /// all the mapping writes (an unlooped stream's end): nothing to do.
    Satisfied {
        /// The fraction of the ring ahead of the reader (0 to 1).
        fill: f32,
    },
    /// Below the threshold with room to write: `room` frames can still land.
    Below {
        /// The fraction of the ring ahead of the reader (0 to 1).
        fill: f32,
        /// Frames the window may still grow by (to most of a ring ahead of
        /// the reader, never past `end`).
        room: u64,
    },
}

/// Whether a ring whose window is `window` (`[from, to)`, in positions) of
/// `frames` slots needs the streamer for a reader at `play`, in a mapping that
/// writes up to `end` (`None`: unbounded).
///
/// A reader a little past the window's end (the refill fell behind) is caught
/// up by appending; only one far ahead, or behind the window, moves it. Moving
/// it is a shrink, which holds back writes the reader's block may cover until
/// it claims again, so moving it every cycle would never let the refill land.
/// A window already parked at the position limit (a reader that ran off to an
/// unbounded position) stays: it reads as a miss, so that voice is silent, and
/// moving it again each cycle would only shrink it again.
///
/// Pure and allocation-free: the audio thread calls it.
#[inline]
pub(crate) fn needs_refill(
    play: u64,
    window: (u64, u64),
    frames: usize,
    end: Option<u64>,
) -> Refill {
    let (from, to) = window;
    let far = (frames / 4) as u64;
    let parked = to >= MAX_POS_RING_POSITION;
    if play < from || (play > to.saturating_add(far) && !parked && end.is_none_or(|end| play < end))
    {
        return Refill::Jumped;
    }
    let frames = frames as u64;
    let ahead = play.saturating_add(frames - frames / 8);
    // Nothing is written at the position limit, so nothing is read for it.
    let limit = end
        .map_or(ahead, |end| end.min(ahead))
        .min(MAX_POS_RING_POSITION);
    let fill = ((to.saturating_sub(play) as f64 / frames as f64) as f32).clamp(0.0, 1.0);
    if fill >= FILL_THRESHOLD || to >= limit {
        Refill::Satisfied { fill }
    } else {
        Refill::Below {
            fill,
            room: limit - to,
        }
    }
}

/// Refill one ring: follow its reader, then fill ahead of it.
///
/// A reader outside the window moves it (see the module docs); a pending
/// switch goes with the old window — the jump is its own discontinuity, and
/// the reader fades across it itself. Then, below the refill threshold, one
/// varifill chunk of the stream's mapping lands at the window's end, up to
/// most of a ring ahead of the reader (and never past what the mapping
/// writes: an unlooped stream's end).
///
/// Says whether a read of the file failed (a decode fault), whether the ring
/// is primed (at or above its refill threshold, or holding all it can be
/// given) and how full it was before this refill wrote anything.
fn refill_one(
    writer: &mut RegionOut,
    cells: &Cells,
    base_chunk_size: usize,
    read_rate: f64,
    buffer: &mut Vec<f32>,
) -> Refilled {
    let ring = Arc::clone(writer.ring());
    let play = writer.play();
    // A reader past what the mapping writes (an unlooped stream's end) plays
    // silence: nothing to follow there.
    let end = writer.content().current.end();
    let mut need = needs_refill(play, writer.window(), writer.frames(), end);
    if need == Refill::Jumped {
        writer.reset(play.saturating_sub(HISTORY_FRAMES));
        if writer.content().switch_at.is_some() {
            let mut content = Content::new(writer.content().current.clone());
            content.epoch = writer.content().epoch + 1;
            ring.publish_map(RingMap::plain(content.current.arrangement(), content.epoch));
            writer.set_content(content);
        }
        need = needs_refill(play, writer.window(), writer.frames(), end);
    }
    let (fill, room) = match need {
        Refill::Satisfied { fill } => {
            return Refilled {
                read_ok: true,
                primed: true,
                fill,
            };
        }
        Refill::Below { fill, room } => (fill, room),
        // A window just reset at the reader's position holds it: not reached.
        Refill::Jumped => {
            return Refilled {
                read_ok: true,
                primed: false,
                fill: 0.0,
            };
        }
    };
    let speed = cells.read_rate().get() as f32;
    let chunk = varifill_chunk(fill, base_chunk_size, read_rate, speed);
    let n = (chunk as u64).min(room) as usize;
    let (_, to) = writer.window();
    let ch = writer.channels().count() as usize;
    buffer.clear();
    buffer.resize(n * ch, 0.0);
    let mapping = writer.content().current.clone();
    let read_ok = writer.fill_with(&mapping, to, buffer);
    writer.push_interleaved(buffer);
    Refilled {
        read_ok,
        primed: false,
        fill,
    }
}

/// What one refill of a ring found.
pub(in crate::streamer) struct Refilled {
    /// No read of the file failed.
    pub(in crate::streamer) read_ok: bool,
    /// The ring was at its refill threshold, or full as far as it goes, before
    /// this refill wrote anything: a voice starting now has frames to read.
    pub(in crate::streamer) primed: bool,
    /// How full the ring was before this refill wrote, as a fraction of its
    /// capacity (0 to 1): what the loop reads to sleep or come straight back.
    pub(in crate::streamer) fill: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `needs_refill`, the one decision the streamer's refill and a voice's
    /// ring share, as a table over a 1000-slot ring (threshold 0.75, so 750
    /// frames ahead; far is 250; a reader may be refilled up to 875 ahead).
    /// Each row is `(label, play, window, end, expected)`.
    ///
    /// Mutation (run): the threshold test `fill >= FILL_THRESHOLD` as `>` ->
    /// the "exactly at threshold" row says `Below`. Mutation (run): the `to >=
    /// limit` arm removed -> the "all of the file is in" and "tail" rows say
    /// `Below`, which is the spin `Stalled` exists to stop. Mutation (run): the
    /// jump test's `play < from` removed -> the "behind" row says `Satisfied`.
    #[test]
    fn needs_refill_follows_its_documented_rule() {
        const F: usize = 1000;
        use Refill::{Below, Jumped, Satisfied};
        #[rustfmt::skip]
        let rows: &[(&str, u64, (u64, u64), Option<u64>, Refill)] = &[
            ("ahead by more than the threshold", 100, (96, 900), None, Satisfied { fill: 0.8 }),
            ("exactly at threshold", 100, (96, 850), None, Satisfied { fill: 0.75 }),
            ("just below threshold", 100, (96, 849), None, Below { fill: 0.749, room: 126 }),
            ("an empty window just behind the reader", 100, (96, 96), None, Below { fill: 0.0, room: 879 }),
            ("behind the window", 90, (96, 900), None, Jumped),
            ("a little past the end: caught up by appending", 1000, (96, 900), None, Below { fill: 0.0, room: 975 }),
            ("far past the end", 1300, (96, 900), None, Jumped),
            ("far past, but past the file's end too: nothing to follow", 1300, (96, 900), Some(900), Satisfied { fill: 0.0 }),
            ("all of the file is in", 100, (96, 400), Some(400), Satisfied { fill: 0.3 }),
            ("the tail: below threshold, nothing left to write", 300, (96, 400), Some(400), Satisfied { fill: 0.1 }),
            ("the tail with frames to come", 300, (96, 400), Some(500), Below { fill: 0.1, room: 100 }),
        ];
        for (label, play, window, end, expected) in rows {
            let got = needs_refill(*play, *window, F, *end);
            match (got, *expected) {
                (Satisfied { fill: a }, Satisfied { fill: b })
                | (Below { fill: a, .. }, Below { fill: b, .. }) => {
                    assert!((a - b).abs() < 1e-3, "{label}: {got:?} vs {expected:?}");
                    if let (Below { room: x, .. }, Below { room: y, .. }) = (got, *expected) {
                        assert_eq!(x, y, "{label}");
                    }
                }
                (g, e) => assert_eq!(g, e, "{label}"),
            }
        }
    }

    /// The whole `varifill_chunk` surface as one table, each row a `(fill,
    /// base, bytes/sec, speed)` input and the exact frame count it must return.
    ///
    /// It is a pure function of four scalars with a closed form, so a table is
    /// the honest shape: sixteen one-point tests said nothing a row does not,
    /// and hid that some of them only asserted an inequality against a
    /// neighbouring call. Every expectation below is computed by hand from the
    /// documented formula
    ///
    /// ```text
    /// multiplier = (0.5 + (1 - fill) * 1.5) * bandwidth * max(speed, 1)
    /// chunk      = max(base * clamp(multiplier, 0.25, 4.0), 1024)
    /// bandwidth  = if rate > 0 { clamp(sqrt(rate / 10 MB/s), 0.5, 2.0) } else { 1.0 }
    /// ```
    ///
    /// so a formula change fails here rather than being absorbed by a
    /// comparison against another call to the same changed function.
    #[test]
    fn varifill_chunk_follows_its_documented_formula() {
        const BASE: usize = 4096;
        const BASELINE: f64 = 10_000_000.0;

        // (label, fill, base, bytes/sec, speed, expected frames)
        let cases: &[(&str, f32, usize, f64, f32, usize)] = &[
            // -- urgency: an emptier ring pulls harder, linearly.
            ("empty ring doubles", 0.0, BASE, BASELINE, 1.0, BASE * 2),
            ("half-full is 1.25x", 0.5, BASE, BASELINE, 1.0, BASE * 5 / 4),
            ("full ring halves", 1.0, BASE, BASELINE, 1.0, BASE / 2),
            // -- speed: faster drains the ring faster; slower is no reason to
            //    read in smaller pieces, so the factor floors at 1.0.
            ("2x speed doubles", 0.5, BASE, BASELINE, 2.0, BASE * 5 / 2),
            (
                "0.5x speed does not shrink",
                0.5,
                BASE,
                BASELINE,
                0.5,
                BASE * 5 / 4,
            ),
            // -- bandwidth: sqrt of the ratio against the 10 MB/s baseline.
            (
                "4x bandwidth doubles",
                0.5,
                BASE,
                BASELINE * 4.0,
                1.0,
                BASE * 5 / 2,
            ),
            (
                "0.25x bandwidth halves",
                0.5,
                BASE,
                BASELINE / 4.0,
                1.0,
                BASE * 5 / 8,
            ),
            // -- bandwidth clamps to 0.5..=2.0 either side.
            (
                "100x bandwidth clamps to 2x",
                0.5,
                BASE,
                BASELINE * 100.0,
                1.0,
                BASE * 5 / 2,
            ),
            (
                "0.01x bandwidth clamps to 0.5x",
                0.5,
                BASE,
                BASELINE / 100.0,
                1.0,
                BASE * 5 / 8,
            ),
            // -- the multiplier itself clamps to 0.25..=4.0.
            (
                "empty + fast disk + 4x speed clamps to 4x",
                0.0,
                BASE,
                BASELINE * 100.0,
                4.0,
                BASE * 4,
            ),
            // -- the 1024-frame floor wins over a small base.
            (
                "a small base floors at 1024",
                1.0,
                100,
                1_000_000.0,
                1.0,
                1024,
            ),
            // -- unmeasured / nonsensical bandwidth contributes a neutral 1.0.
            (
                "zero bandwidth is neutral",
                0.5,
                BASE,
                0.0,
                1.0,
                BASE * 5 / 4,
            ),
            (
                "negative bandwidth is neutral",
                0.5,
                BASE,
                -1000.0,
                1.0,
                BASE * 5 / 4,
            ),
            (
                "infinite bandwidth clamps to 2x",
                0.5,
                BASE,
                f64::INFINITY,
                1.0,
                BASE * 5 / 2,
            ),
            // -- a nonsensical fill is bounded by the clamp and the floor.
            //    fill > 1 gives a negative urgency, clamped up to 0.25.
            (
                "fill above one clamps to 0.25x",
                1.5,
                BASE,
                BASELINE,
                1.0,
                BASE / 4,
            ),
            //    fill < 0 gives urgency 1.5, so 2.75x -- under the 4.0 ceiling.
            (
                "negative fill is 2.75x",
                -0.5,
                BASE,
                BASELINE,
                1.0,
                BASE * 11 / 4,
            ),
        ];

        for &(label, fill, base, rate, speed, want) in cases {
            let got = varifill_chunk(fill, base, rate, speed);
            assert_eq!(
                got, want,
                "{label}: varifill_chunk returned {got}, want {want}"
            );
        }

        // NaN has no arithmetic expectation -- `f64::clamp` propagates it and
        // the `as usize` cast saturates to 0 -- so it is asserted against the
        // floor, which is the only guarantee the function can make here.
        let nan = varifill_chunk(f32::NAN, BASE, BASELINE, 1.0);
        assert!(
            nan >= 1024,
            "a NaN fill must still respect the 1024 floor, got {nan}"
        );
    }

    /// A voice whose reader sits at an unbounded position (the audio thread's
    /// saturating cast turns `inf` into a huge `u64`) must neither panic the
    /// streamer nor make it read the disk or move the window again each cycle:
    /// its window parks empty at the position limit, so its reads miss and
    /// that voice is silent. (The test ring's length is unknown, so its end
    /// is `u64::MAX`; the reader sits one frame short of it. A looped stream
    /// has no end and takes the same branch.)
    ///
    /// Mutation (run): drop the `.min(MAX_POS_RING_POSITION)` on `limit` ->
    /// the cycle reads a chunk for nothing and `buffer` is non-empty -> fails.
    /// Mutation (run): drop `!parked` -> the second cycle moves the window
    /// again and `moves()` grows -> fails. Mutation (run): `PosWriter::reset`
    /// without its saturation -> the first cycle panics -> fails.
    #[test]
    fn a_reader_at_an_unbounded_position_parks_the_window() {
        use super::super::super::prefetch::RegionBuffer;
        use std::path::PathBuf;

        let (mut writer, ring, mut reader) =
            RegionBuffer::with_capacity(StreamId(1), PathBuf::from("t.wav"), 4096, 1u16);
        let cells = Cells::new();
        let mut buffer = Vec::new();
        for _ in 0..3 {
            drop(reader.claim(u64::MAX - 1, [(u64::MAX - 6, u64::MAX - 1), (0, 0)]));
            refill_one(&mut writer, &cells, 4096, 0.0, &mut buffer);
        }
        assert_eq!(
            writer.window(),
            (MAX_POS_RING_POSITION, MAX_POS_RING_POSITION)
        );
        assert!(buffer.is_empty(), "nothing is read for a parked window");
        assert_eq!(ring.moves(), 1, "the window moved once, then stayed");

        // The voice coming back is followed again.
        drop(reader.claim(0, [(0, 4), (0, 0)]));
        refill_one(&mut writer, &cells, 4096, 0.0, &mut buffer);
        assert_eq!(writer.window().0, 0);
    }
}
