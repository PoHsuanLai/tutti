//! Ring buffer refill logic for butler thread.
//!
//! Decides, per streaming channel and per cycle, whether the ring needs frames
//! and how many to move — then moves them, forward or reversed, from an
//! incremental disk decoder or from a resident whole-file [`Wave`].
//!
//! **Every count here is denominated in frames.** `chunk_size`, `write_space`
//! and `file_position` are all frame counts; the `* ch` that appears at each
//! scratch-buffer resize is the only place the interleave stride enters. A
//! sample-denominated `chunk_size` would over-request by the channel count and
//! desynchronise `file_position` from the loop range it is compared against.

use super::super::cache::LruCache;
use super::super::metrics::Metrics;
use super::super::plan::ChannelPlan;
use super::super::prefetch::RegionOut;
use super::super::region_map::RegionMap;
use super::wave_io::{wave_frame_into, wrap_position, WaveIn};
use dashmap::DashMap;
use rayon::prelude::*;
use std::path::{Path, PathBuf};
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
/// Publishes each ring's fill level to its `RtState`, skips the channels already
/// at or above the refill threshold, and sizes the rest through
/// [`varifill_chunk`]. A region carrying a decoder streams the range from disk;
/// one without falls back to the resident whole-file [`Wave`].
///
/// `interleave_buffer` is the butler's reusable scratch, passed in so a refill
/// does not allocate per cycle. Butler thread throughout — this both blocks on
/// disk and may grow that buffer, so it must never run on the audio thread.
pub(crate) fn refill_all(
    plans: &DashMap<usize, ChannelPlan>,
    regions: &mut RegionMap,
    cache: &LruCache,
    metrics: &Metrics,
    base_chunk_size: usize,
    buffer_margin: f64,
    interleave_buffer: &mut Vec<f32>,
) {
    let read_rate = metrics.read_rate();

    for entry in plans.iter() {
        let stream_state = entry.value();

        let Some(link) = stream_state.link.as_ref() else {
            continue;
        };

        let Some(writer) = regions.get_mut(link.region_id) else {
            continue;
        };

        let fill_pct = writer.buffered().get() as f32 / writer.capacity().get() as f32;

        stream_state.rt_state.set_buffer_fill(fill_pct);

        let fill_threshold = (0.75 / buffer_margin) as f32;

        if fill_pct >= fill_threshold {
            continue;
        }

        let is_reverse = stream_state.rt_state.is_reverse();
        let adjusted_speed = stream_state.rt_state.read_rate().get() as f32 * buffer_margin as f32;
        let chunk_size = varifill_chunk(fill_pct, base_chunk_size, read_rate, adjusted_speed);

        let file_position = writer.file_position() as usize;
        let loop_range = stream_state.loop_config().map(|c| c.range);

        // Real incremental streaming: decode only the requested range from
        // disk. Regions whose format isn't seekable have no decoder and use the
        // whole-file `load_wave` + `LruCache` fallback below.
        #[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
        if writer.decoder_mut().is_some() {
            if is_reverse {
                refill_reverse_stream(writer, file_position, chunk_size, interleave_buffer);
            } else {
                refill_forward_stream(
                    writer,
                    file_position,
                    chunk_size,
                    interleave_buffer,
                    loop_range,
                );
            }
            continue;
        }

        let file_path = writer.file_path();
        let Some(wave) = load_wave(cache, metrics, file_path) else {
            continue;
        };

        if is_reverse {
            refill_reverse(writer, &wave, file_position, chunk_size, interleave_buffer);
        } else {
            refill_forward(
                writer,
                &wave,
                file_position,
                chunk_size,
                interleave_buffer,
                loop_range,
            );
        }
    }
}

/// One channel's refill decision, snapshotted before the parallel pass so no
/// plan reference is held across it.
struct RefillWorkItem {
    /// Position in the region `Vec`, which is what `par_iter_mut` enumerates.
    writer_idx: usize,
    /// Frames to read this cycle, from [`varifill_chunk`].
    chunk_size: usize,
    is_reverse: bool,
    file_path: PathBuf,
    /// Ring occupancy at decision time, republished to `RtState` by the worker.
    fill_pct: f32,
    shared: Arc<super::super::rt_state::RtState>,
    /// Carried per item so the parallel path wraps at the loop bounds exactly
    /// like the serial one. Hardcoding `None` here would silently drop loop
    /// handling for every session with 3+ concurrent streams — the very
    /// threshold that selects this path.
    loop_range: Option<(u64, u64)>,
}

/// The same refill across rayon workers, chosen when `parallel_io` is on and
/// three or more channels are streaming.
///
/// Work items are collected first — one per channel that is actually below its
/// refill threshold — so every plan reference is released before the parallel
/// pass begins. `par_iter_mut` over the region `Vec` then hands each worker an
/// exclusive `&mut` to a *different* producer, which is why the regions are a
/// `Vec` rather than a `DashMap`: this needs `Send`, not `Sync`. Scratch is a
/// thread-local per worker.
///
/// Each item carries its own `loop_range`, so a looped stream wraps here exactly
/// as it does serially. Dropping that would make looping depend on how many
/// voices happened to be streaming.
pub(crate) fn refill_all_parallel(
    plans: &DashMap<usize, ChannelPlan>,
    regions: &mut RegionMap,
    cache: &LruCache,
    metrics: &Metrics,
    base_chunk_size: usize,
    buffer_margin: f64,
) {
    let read_rate = metrics.read_rate();

    let fill_threshold = (0.75 / buffer_margin) as f32;

    let work_items: Vec<RefillWorkItem> = plans
        .iter()
        .filter_map(|entry| {
            let stream_state = entry.value();
            let link = stream_state.link.as_ref()?;
            let idx = *regions.index().get(&link.region_id)?;
            let writer = regions.writers().get(idx)?;

            let fill_pct = writer.buffered().get() as f32 / writer.capacity().get() as f32;

            if fill_pct >= fill_threshold {
                return None;
            }

            let adjusted_speed =
                stream_state.rt_state.read_rate().get() as f32 * buffer_margin as f32;
            let chunk_size = varifill_chunk(fill_pct, base_chunk_size, read_rate, adjusted_speed);

            let shared = stream_state.rt_state();

            Some(RefillWorkItem {
                writer_idx: idx,
                chunk_size,
                is_reverse: stream_state.rt_state.is_reverse(),
                file_path: writer.file_path().to_path_buf(),
                fill_pct,
                shared,
                loop_range: stream_state.loop_config().map(|c| c.range),
            })
        })
        .collect();

    let work_by_idx: std::collections::HashMap<usize, &RefillWorkItem> = work_items
        .iter()
        .map(|item| (item.writer_idx, item))
        .collect();

    regions
        .writers_mut()
        .par_iter_mut()
        .enumerate()
        .for_each(|(idx, writer)| {
            let Some(item) = work_by_idx.get(&idx) else {
                return;
            };

            thread_local! {
                static LOCAL_BUF: std::cell::RefCell<Vec<f32>> =
                    std::cell::RefCell::new(Vec::with_capacity(16384));
            }

            LOCAL_BUF.with(|buf| {
                let mut buf = buf.borrow_mut();
                refill_one(
                    writer,
                    cache,
                    item.chunk_size,
                    item.is_reverse,
                    &item.file_path,
                    item.fill_pct,
                    &item.shared,
                    &mut buf,
                    item.loop_range,
                );
            });
        });
}

/// Refill one stream from a work item, on a rayon worker.
///
/// Takes the writer as a direct `&mut` straight from `par_iter_mut` — the
/// exclusivity that makes the parallel pass sound is the borrow itself, so this
/// never looks a region up by id. `buffer` is the worker's thread-local scratch.
fn refill_one(
    writer: &mut RegionOut,
    cache: &LruCache,
    chunk_size: usize,
    is_reverse: bool,
    file_path: &Path,
    fill_pct: f32,
    shared: &super::super::rt_state::RtState,
    buffer: &mut Vec<f32>,
    loop_range: Option<(u64, u64)>,
) {
    shared.set_buffer_fill(fill_pct);

    let file_position = writer.file_position() as usize;

    // Real incremental streaming when this region has a decoder. `loop_range`
    // comes from the work item so this matches the serial path.
    #[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
    if writer.decoder_mut().is_some() {
        if is_reverse {
            refill_reverse_stream(writer, file_position, chunk_size, buffer);
        } else {
            refill_forward_stream(writer, file_position, chunk_size, buffer, loop_range);
        }
        return;
    }

    let Some(wave) = cache.get(file_path) else {
        return;
    };

    if is_reverse {
        refill_reverse(writer, &wave, file_position, chunk_size, buffer);
    } else {
        // Whole-file forward via the WaveIn source into the region ring. WaveIn
        // zero-pads past end, so one block fill of `chunk_size` frames always
        // produces a full buffer. `loop_range` is honoured here for the same
        // reason as the decoder path above: this function serves the 3+-stream
        // parallel refill, and dropping it there would make looping depend on
        // how many voices happened to be streaming.
        refill_forward(writer, &wave, file_position, chunk_size, buffer, loop_range);
    }
}

/// Refill for forward playback by decoding straight from disk (real streaming),
/// respecting loop boundaries if set. Advances `file_position` by the frames
/// written — identical bookkeeping to the whole-file [`refill_forward`].
///
/// The decoder is a sequential [`AudioIn`](tutti_core::io::AudioIn): each run
/// seeks only when the target `pos` differs from the decoder's cursor, then
/// polls forward. Without a loop that is one seek-free sequential fill; with a
/// loop, each run stops at `loop_end` and the next seeks back to `loop_start`.
#[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
fn refill_forward_stream(
    writer: &mut RegionOut,
    file_position: usize,
    chunk_size: usize,
    interleave_buffer: &mut Vec<f32>,
    loop_range: Option<(u64, u64)>,
) {
    // Stride derived once per refill, above every loop below.
    let ch = writer.channels().count() as usize;
    interleave_buffer.clear();
    interleave_buffer.resize(chunk_size * ch, 0.0);

    // Active loop end, if the range is non-empty (used to cap each run).
    let loop_end = loop_range.and_then(|(start, end)| (end > start).then_some(end as usize));

    let decoder = match writer.decoder_mut() {
        Some(d) => d,
        None => return,
    };

    let mut filled = 0usize;
    let mut pos = file_position;
    while filled < chunk_size {
        let run = if let Some(loop_end) = loop_end {
            pos = wrap_position(pos, loop_range);
            (loop_end - pos).min(chunk_size - filled)
        } else {
            chunk_size - filled
        };
        // Seek only when the decoder isn't already positioned here (the common
        // no-loop path stays seek-free after the first fill).
        if decoder.cursor() != pos as u64 {
            let _ = decoder.seek(pos as u64);
        }
        let got = decoder
            .fill_sequential_interleaved(&mut interleave_buffer[filled * ch..(filled + run) * ch])
            .unwrap_or(0);
        // Past EOF the decoder yields a short count; zero-fill the rest of this
        // run to preserve the old zero-pad behaviour and keep bookkeeping simple.
        interleave_buffer[(filled + got) * ch..(filled + run) * ch].fill(0.0);
        filled += run;
        pos += run;
    }

    let written = writer.push_interleaved(interleave_buffer);

    // `written` is FRAMES (a `Samples`), added to a file position in frames.
    // Were it the interleaved sample count, a 6-channel loop would wrap at a
    // sixth of its length — see `a_six_channel_loop_wraps_at_its_frame_length`.
    let new_pos = wrap_position(file_position + written.get(), loop_range);
    writer.set_file_position(new_pos as u64);
}

/// Refill for reverse playback by decoding forward from disk then writing the
/// frames reversed — mirrors [`refill_reverse`] but streams the range instead of
/// reading a resident `Wave`. `pump` can't express reversal, so this stays
/// hand-rolled.
#[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
fn refill_reverse_stream(
    writer: &mut RegionOut,
    file_position: usize,
    chunk_size: usize,
    interleave_buffer: &mut Vec<f32>,
) {
    let read_start = file_position.saturating_sub(chunk_size);
    let actual_chunk = file_position - read_start;

    // Stride derived once per refill, above every loop below.
    let ch = writer.channels().count() as usize;
    if actual_chunk == 0 {
        interleave_buffer.clear();
        interleave_buffer.resize(chunk_size * ch, 0.0);
        writer.push_interleaved(interleave_buffer);
        return;
    }

    interleave_buffer.clear();
    interleave_buffer.resize(actual_chunk * ch, 0.0);

    if let Some(decoder) = writer.decoder_mut() {
        if decoder.cursor() != read_start as u64 {
            let _ = decoder.seek(read_start as u64);
        }
        let got = decoder
            .fill_sequential_interleaved(&mut interleave_buffer[..])
            .unwrap_or(0);
        interleave_buffer[got * ch..].fill(0.0);
    }

    // NOT `interleave_buffer.reverse()`: on a flat buffer that reverses
    // individual SAMPLES, swapping every channel pair within each frame. It is
    // only equivalent when the element type is itself a frame, which here it is
    // not. `write_interleaved_reversed` reverses the frame sequence and keeps
    // channels in order within each frame.
    let written = writer.write_interleaved_reversed(interleave_buffer);
    writer.set_file_position(file_position.saturating_sub(written.get()) as u64);
}

/// Refill for forward playback from a resident `Wave`, respecting loop
/// boundaries if set. Fills one `chunk_size` block through the [`WaveIn`]
/// source (mono up-mix + loop wrap + zero-pad past end confined there) and
/// pushes it into the region ring.
///
/// Pushes through the inherent `push_interleaved` rather than
/// [`AudioOut::write`](tutti_core::AudioOut::write), which `RegionOut` also
/// implements: `write` returns `()`, and the landed frame count is exactly what
/// advances `file_position` below. This is the concrete case behind that
/// impl's note that the inherent method stays the one production callers use.
fn refill_forward(
    writer: &mut RegionOut,
    wave: &Wave,
    file_position: usize,
    chunk_size: usize,
    interleave_buffer: &mut Vec<f32>,
    loop_range: Option<(u64, u64)>,
) {
    // Stride derived once per refill, above every loop below.
    let ch = writer.channels().count() as usize;
    interleave_buffer.clear();
    interleave_buffer.resize(chunk_size * ch, 0.0);

    let mut src = WaveIn::new(wave, file_position, loop_range, ch);
    src.fill_interleaved(interleave_buffer);

    let written = writer.push_interleaved(interleave_buffer);

    // `written` is FRAMES (a `Samples`), added to a file position in frames.
    // Were it the interleaved sample count, a 6-channel loop would wrap at a
    // sixth of its length — see `a_six_channel_loop_wraps_at_its_frame_length`.
    let new_pos = wrap_position(file_position + written.get(), loop_range);
    writer.set_file_position(new_pos as u64);
}

/// Refill for reverse playback from a resident `Wave`. Reads frames forward
/// through [`wave_frame_into`], then pushes them reversed into the ring.
/// `pump` cannot express reversal, so this stays hand-rolled.
fn refill_reverse(
    writer: &mut RegionOut,
    wave: &Wave,
    file_position: usize,
    chunk_size: usize,
    interleave_buffer: &mut Vec<f32>,
) {
    let read_start = file_position.saturating_sub(chunk_size);
    let actual_chunk = file_position - read_start;

    // Stride derived once per refill, above every loop below.
    let ch = writer.channels().count() as usize;
    if actual_chunk == 0 {
        interleave_buffer.clear();
        interleave_buffer.resize(chunk_size * ch, 0.0);
        writer.push_interleaved(interleave_buffer);
        return;
    }

    interleave_buffer.clear();
    interleave_buffer.resize(actual_chunk * ch, 0.0);
    for (i, frame) in interleave_buffer.chunks_exact_mut(ch).enumerate() {
        wave_frame_into(wave, read_start + i, frame);
    }

    let written = writer.write_interleaved_reversed(interleave_buffer);
    writer.set_file_position(file_position.saturating_sub(written.get()) as u64);
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

    fn make_test_wave(samples: &[(f32, f32)]) -> Wave {
        let mut wave = Wave::new(2, 48000.0);
        for (l, r) in samples {
            wave.push_frame(&[*l, *r]);
        }
        wave
    }

    /// **A 6-channel loop wraps at its full FRAME length**, and a reverse
    /// refill steps back by frames — the failure `CLAUDE.md` names for this
    /// boundary ("a 6-channel looped clip wraps at a sixth of its length").
    ///
    /// Both refills advance `file_position` by the count the ring reports
    /// landing. That count is a `Samples` now, so a *caller* can no longer hand
    /// in the interleaved length by accident; what remains checkable at runtime
    /// is that the ring itself reports frames, end to end through the real
    /// refill arithmetic. At six channels a sample count is a 6× error, which
    /// the loop wrap turns into a visibly wrong position rather than a
    /// plausible one.
    ///
    /// Mutation: `push_interleaved` returning `Samples(samples.len())` (the
    /// interleaved length) → 900 "frames" land, `wrap(900) = 0`, not 50 → the
    /// forward assertion fails. `write_interleaved_reversed` returning the
    /// sample count → 180 back from 100 saturates to 0, not 70 → the reverse
    /// assertion fails.
    #[test]
    fn a_six_channel_loop_wraps_at_its_frame_length() {
        use crate::butler::command::RegionId;
        use crate::butler::RegionBuffer;
        use tutti_core::Samples;

        const CH: usize = 6;
        const LOOP_FRAMES: usize = 100;
        let mut wave = Wave::zero(CH, 48_000.0, LOOP_FRAMES as f64 / 48_000.0);
        for i in 0..LOOP_FRAMES {
            for c in 0..CH {
                wave.set(c, i, (i * CH + c) as f32 / 1000.0);
            }
        }
        assert_eq!(wave.len(), LOOP_FRAMES);
        let mut buf = Vec::new();

        // Forward: 150 frames from frame 0 of a 100-frame loop.
        let (mut writer, _reader) =
            RegionBuffer::with_capacity(RegionId(1), PathBuf::new(), 4096, CH);
        refill_forward(
            &mut writer,
            &wave,
            0,
            150,
            &mut buf,
            Some((0, LOOP_FRAMES as u64)),
        );
        assert_eq!(writer.buffered(), Samples(150), "150 frames landed");
        assert_eq!(
            writer.file_position(),
            50,
            "150 frames into a 100-frame loop is frame 50 — a sample-denominated \
             count (900) would have wrapped to 0"
        );

        // Reverse: 30 frames back from frame 100.
        let (mut writer, _reader) =
            RegionBuffer::with_capacity(RegionId(2), PathBuf::new(), 4096, CH);
        refill_reverse(&mut writer, &wave, LOOP_FRAMES, 30, &mut buf);
        assert_eq!(
            writer.file_position(),
            70,
            "30 frames back from 100 is 70 — a sample count (180) would saturate to 0"
        );
    }

    #[test]
    fn test_forward_fill_via_wave_in() {
        // The forward whole-file fill now runs through WaveIn; verify the block
        // it produces matches the old fill_buffer_forward output shape.
        let wave = make_test_wave(&[(0.1, 0.1), (0.2, 0.2), (0.3, 0.3), (0.4, 0.4)]);
        let mut src = WaveIn::new(&wave, 0, None, 2usize);
        let mut buffer = vec![0.0f32; 3 * 2];
        src.fill_interleaved(&mut buffer);

        assert_eq!(buffer, [0.1, 0.1, 0.2, 0.2, 0.3, 0.3]);
    }

    #[test]
    fn test_forward_fill_past_end_pads_zeros() {
        let wave = make_test_wave(&[(0.1, 0.1), (0.2, 0.2)]);
        let mut src = WaveIn::new(&wave, 1, None, 2usize);
        let mut buffer = vec![9.0f32; 4 * 2];
        src.fill_interleaved(&mut buffer);

        assert_eq!(&buffer[0..2], [0.2, 0.2]); // Last valid sample
        assert_eq!(&buffer[2..4], [0.0, 0.0]); // Past end - zeros
        assert_eq!(&buffer[4..6], [0.0, 0.0]);
        assert_eq!(&buffer[6..8], [0.0, 0.0]);
    }

    #[test]
    fn test_loop_wrap_position_calculation() {
        // Test the wrapping logic used in refill_forward_loop_aware
        let loop_start = 100usize;
        let loop_end = 200usize;
        let loop_len = loop_end - loop_start;

        // Position at exactly loop_end should wrap to loop_start
        let pos = 200usize;
        let wrapped = if pos >= loop_end {
            loop_start + ((pos - loop_start) % loop_len)
        } else {
            pos
        };
        assert_eq!(wrapped, loop_start);

        // Position past loop_end should wrap correctly
        let pos = 250usize;
        let wrapped = if pos >= loop_end {
            loop_start + ((pos - loop_start) % loop_len)
        } else {
            pos
        };
        // 250 - 100 = 150, 150 % 100 = 50, 100 + 50 = 150
        assert_eq!(wrapped, 150);

        // Position 2 full loops past should wrap back
        let pos = 300usize;
        let wrapped = if pos >= loop_end {
            loop_start + ((pos - loop_start) % loop_len)
        } else {
            pos
        };
        // 300 - 100 = 200, 200 % 100 = 0, 100 + 0 = 100
        assert_eq!(wrapped, loop_start);
    }

    #[test]
    fn test_loop_wrap_does_not_affect_position_before_loop_end() {
        let loop_start = 100usize;
        let loop_end = 200usize;

        // Position before loop_end should not be affected
        for pos in [100, 150, 199] {
            let wrapped = if pos >= loop_end {
                loop_start + ((pos - loop_start) % (loop_end - loop_start))
            } else {
                pos
            };
            assert_eq!(wrapped, pos, "Position {} should not be wrapped", pos);
        }
    }
}
