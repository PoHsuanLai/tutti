//! Ring refill for the butler thread: keep each ring filled ahead of where
//! its reader plays.
//!
//! The reader publishes the straight position it plays (`Ring::play`); the
//! refill keeps the ring's window `[from, to)` running from just behind it to
//! most of the ring ahead of it, in the stream's mapping (`loops`). A reader
//! that has jumped outside the window — a seek, a PDC change, a varispeed
//! change, a transport loop — moves the window: it is emptied at the reader's
//! position and filled from there. Nothing is flushed; frames already written
//! stay valid for their positions, so a jump back inside the window costs
//! nothing.
//!
//! **Every count here is denominated in frames.** The `* ch` at each scratch
//! resize is the only place the interleave stride enters.

use super::super::cache::LruCache;
use super::super::loops::{Content, RingMap};
use super::super::metrics::Metrics;
use super::super::plan::ChannelPlan;
use super::super::prefetch::{RegionOut, HISTORY_FRAMES};
use super::super::region_map::RegionMap;
use super::super::rt_state::RtState;
use dashmap::DashMap;
use rayon::prelude::*;
use std::path::Path;
use std::sync::Arc;
use tutti_io::Wave;

/// How many **frames** to read this cycle, under the varifill strategy.
///
/// Scales `base_chunk` by three independent factors, then clamps their product
/// to `0.25..=4.0` and floors the result at 1024 frames:
///
/// - **Urgency** — `1.0 - buffer_fill`, so an empty ring pulls harder.
/// - **Bandwidth** — the square root of `read_rate_bytes_per_sec` against a
///   10 MB/s baseline, clamped to `0.5..=2.0`. A non-positive or unmeasured rate
///   contributes a neutral 1.0.
/// - **Speed** — `playback_speed`, floored at 1.0: playing faster consumes the
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

/// Refill every streaming channel's ring in turn, serially.
///
/// `interleave_buffer` is the butler's reusable scratch, passed in so a refill
/// does not allocate per cycle. Butler thread throughout — this both blocks on
/// disk and may grow that buffer, so it must never run on the audio thread.
pub(crate) fn refill_all(
    plans: &DashMap<usize, ChannelPlan>,
    regions: &mut RegionMap,
    metrics: &Metrics,
    base_chunk_size: usize,
    buffer_margin: f64,
    interleave_buffer: &mut Vec<f32>,
) {
    let read_rate = metrics.read_rate();
    for entry in plans.iter() {
        let plan = entry.value();
        let Some(link) = plan.link.as_ref() else {
            continue;
        };
        let Some(writer) = regions.get_mut(link.region_id) else {
            continue;
        };
        refill_one(
            writer,
            &plan.rt_state,
            base_chunk_size,
            buffer_margin,
            read_rate,
            interleave_buffer,
        );
    }
}

/// The same refill across rayon workers, chosen when `parallel_io` is on and
/// three or more channels are streaming.
///
/// Work items (the writer's index and its channel's `RtState`) are collected
/// first, so every plan reference is released before the parallel pass;
/// `par_iter_mut` over the region `Vec` then hands each worker an exclusive
/// `&mut` to a *different* writer, whose mapping — loop included — travels
/// with it, so a looped stream refills here exactly as it does serially.
/// Scratch is a thread-local per worker.
pub(crate) fn refill_all_parallel(
    plans: &DashMap<usize, ChannelPlan>,
    regions: &mut RegionMap,
    metrics: &Metrics,
    base_chunk_size: usize,
    buffer_margin: f64,
) {
    let read_rate = metrics.read_rate();
    let work: std::collections::HashMap<usize, Arc<RtState>> = plans
        .iter()
        .filter_map(|entry| {
            let plan = entry.value();
            let link = plan.link.as_ref()?;
            let idx = *regions.index().get(&link.region_id)?;
            Some((idx, plan.rt_state()))
        })
        .collect();

    regions
        .writers_mut()
        .par_iter_mut()
        .enumerate()
        .for_each(|(idx, writer)| {
            let Some(rt_state) = work.get(&idx) else {
                return;
            };
            thread_local! {
                static LOCAL_BUF: std::cell::RefCell<Vec<f32>> =
                    std::cell::RefCell::new(Vec::with_capacity(16384));
            }
            LOCAL_BUF.with(|buf| {
                refill_one(
                    writer,
                    rt_state,
                    base_chunk_size,
                    buffer_margin,
                    read_rate,
                    &mut buf.borrow_mut(),
                );
            });
        });
}

/// Refill one ring: follow its reader, then fill ahead of it.
///
/// A reader outside the window moves it (see the module docs); a pending
/// switch goes with the old window — the jump is its own discontinuity, and
/// the reader fades across it itself. Then, below the refill threshold, one
/// varifill chunk of the stream's mapping lands at the window's end, up to
/// most of a ring ahead of the reader (and never past what the mapping
/// writes: an unlooped stream's end).
fn refill_one(
    writer: &mut RegionOut,
    rt_state: &RtState,
    base_chunk_size: usize,
    buffer_margin: f64,
    read_rate: f64,
    buffer: &mut Vec<f32>,
) {
    let ring = Arc::clone(writer.ring());
    let play = ring.play();
    let (from, to) = ring.window();
    // A reader past what the mapping writes (an unlooped stream's end) plays
    // silence: nothing to follow there.
    let end = writer.content().current.end();
    if play < from || (play > to && play < end) {
        ring.reset(play.saturating_sub(HISTORY_FRAMES));
        if writer.content().switch_at != 0 {
            let mut content = Content::new(writer.content().current.clone());
            content.epoch = writer.content().epoch + 1;
            ring.publish_map(RingMap::plain(content.current.arrangement(), content.epoch));
            writer.set_content(content);
        }
    }
    let (_, to) = ring.window();
    let frames = ring.frames() as u64;
    let limit = end.min(play + frames - frames / 8);
    let fill = (to.saturating_sub(play) as f64 / frames as f64) as f32;
    rt_state.set_buffer_fill(fill);
    let fill_threshold = (0.75 / buffer_margin) as f32;
    if fill >= fill_threshold || to >= limit {
        return;
    }
    let speed = rt_state.read_rate().get() as f32 * buffer_margin as f32;
    let chunk = varifill_chunk(fill, base_chunk_size, read_rate, speed);
    let n = (chunk as u64).min(limit - to) as usize;
    let ch = writer.channels().count() as usize;
    buffer.clear();
    buffer.resize(n * ch, 0.0);
    let mapping = writer.content().current.clone();
    writer.fill_with(&mapping, to, buffer);
    writer.push_interleaved(buffer);
}

/// The whole-file fallback: hand back a cached [`Wave`], or decode one and cache
/// it. `None` means the file could not be turned into audio — every caller
/// already treats that as "skip this region".
///
/// With no codec feature enabled there is no decoder to call, so the miss arm is
/// gated out and a miss is simply `None`. The cache *lookup* stays live in both
/// builds: entries reach it from elsewhere, and a hit needs no decoder.
pub(in crate::butler) fn load_wave(
    cache: &LruCache,
    metrics: &Metrics,
    file_path: &Path,
) -> Option<Arc<Wave>> {
    if let Some(cached) = cache.get(file_path) {
        return Some(cached);
    }

    // `Wave::load` is `tutti-io`'s decode path, which is compiled only when a
    // codec feature is on — calling it unconditionally breaks the
    // `--no-default-features` build.
    #[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
    {
        if let Ok(w) = Wave::load(file_path) {
            let arc_wave = Arc::new(w);
            let bytes = arc_wave.len() as u64 * arc_wave.channels() as u64 * 4;
            metrics.record_read(bytes);
            cache.insert(file_path.to_path_buf(), arc_wave.clone());
            return Some(arc_wave);
        }
    }

    // Silences the unused-parameter warning in the codec-free build, where the
    // only way to reach this line is a cache miss with nothing able to fill it.
    #[cfg(not(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg")))]
    let _ = metrics;

    None
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
