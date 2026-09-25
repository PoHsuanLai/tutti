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
//!
//! A forward refill writes a looped stream's frames as the continuous sequence
//! the loop plays, fade blended in, through [`fill_sequence`]; `file_position`
//! counts the file straight on and the loop places it (see `loops`' module
//! docs). A reverse refill ignores the loop, on every tier: its cursor is a
//! file frame, and a stream turned round after it has been round its loop
//! reads back from its straight count, as the memory tier's reverse mirrors its
//! clock's position without placing it.

use super::super::cache::LruCache;
use super::super::loops::{fill_sequence, forward_loop, RingLoop};
use super::super::metrics::Metrics;
use super::super::plan::ChannelPlan;
use super::super::prefetch::RegionOut;
use super::super::region_map::RegionMap;
use super::wave_io::{wave_frame_into, WaveIn};
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
        let ring_loop = forward_loop(stream_state);

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
                    ring_loop,
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
                ring_loop,
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
    /// Carried per item so the parallel path writes a loop exactly like the
    /// serial one. Hardcoding `None` here would silently drop loop handling
    /// for every session with 3+ concurrent streams — the very threshold that
    /// selects this path. A clone shares the lead-in (`Arc`).
    ring_loop: Option<RingLoop>,
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
/// Each item carries its own loop, so a looped stream wraps here exactly as it
/// does serially. Dropping that would make looping depend on how many
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
                ring_loop: forward_loop(stream_state).cloned(),
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
                    item.ring_loop.as_ref(),
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
    ring_loop: Option<&RingLoop>,
) {
    shared.set_buffer_fill(fill_pct);

    let file_position = writer.file_position() as usize;

    // Real incremental streaming when this region has a decoder. The loop
    // comes from the work item so this matches the serial path.
    #[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
    if writer.decoder_mut().is_some() {
        if is_reverse {
            refill_reverse_stream(writer, file_position, chunk_size, buffer);
        } else {
            refill_forward_stream(writer, file_position, chunk_size, buffer, ring_loop);
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
        // produces a full buffer. The loop is honoured here for the same
        // reason as the decoder path above: this function serves the 3+-stream
        // parallel refill, and dropping it there would make looping depend on
        // how many voices happened to be streaming.
        refill_forward(writer, &wave, file_position, chunk_size, buffer, ring_loop);
    }
}

/// Refill for forward playback by decoding straight from disk (real streaming),
/// writing a looped stream's sequence ([`fill_sequence`]). Advances
/// `file_position` by the frames written — identical bookkeeping to the
/// whole-file [`refill_forward`].
///
/// The decoder is a sequential [`AudioIn`](tutti_core::io::AudioIn): each run
/// seeks only when its first frame differs from the decoder's cursor, then
/// polls forward. Without a loop that is one seek-free sequential fill; with a
/// loop, each run stops at the loop's end and the next seeks back to where the
/// loop resumes.
#[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
fn refill_forward_stream(
    writer: &mut RegionOut,
    file_position: usize,
    chunk_size: usize,
    interleave_buffer: &mut Vec<f32>,
    ring_loop: Option<&RingLoop>,
) {
    // Stride derived once per refill, above every loop below.
    let ch = writer.channels().count() as usize;
    interleave_buffer.clear();
    interleave_buffer.resize(chunk_size * ch, 0.0);

    let decoder = match writer.decoder_mut() {
        Some(d) => d,
        None => return,
    };

    fill_sequence(
        file_position,
        interleave_buffer,
        ch,
        ring_loop,
        |at, run| {
            // Seek only when the decoder isn't already positioned here (the common
            // no-loop path stays seek-free after the first fill).
            if decoder.cursor() != at as u64 {
                let _ = decoder.seek(at as u64);
            }
            let got = decoder.fill_sequential_interleaved(run).unwrap_or(0);
            // Past EOF the decoder yields a short count; zero-fill the rest of this
            // run to preserve the old zero-pad behaviour and keep bookkeeping simple.
            run[got * ch..].fill(0.0);
        },
    );

    let written = writer.push_interleaved(interleave_buffer);

    // `written` is FRAMES (a `Samples`), added to a file position in frames.
    // Were it the interleaved sample count, a 6-channel stream would run six
    // times ahead of what it wrote — see `a_six_channel_loop_wraps_at_its_frame_length`.
    writer.set_file_position((file_position + written.get()) as u64);
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

/// Refill for forward playback from a resident `Wave`, writing a looped
/// stream's sequence ([`fill_sequence`]): one `chunk_size` block through the
/// [`WaveIn`] source (mono up-mix and zero-pad past end confined there), pushed
/// into the region ring.
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
    ring_loop: Option<&RingLoop>,
) {
    // Stride derived once per refill, above every loop below.
    let channels = writer.channels();
    let ch = channels.count() as usize;
    interleave_buffer.clear();
    interleave_buffer.resize(chunk_size * ch, 0.0);

    fill_sequence(
        file_position,
        interleave_buffer,
        ch,
        ring_loop,
        |at, run| {
            WaveIn::new(wave, at, channels).fill_interleaved(run);
        },
    );

    let written = writer.push_interleaved(interleave_buffer);

    // `written` is FRAMES (a `Samples`), added to a file position in frames.
    // Were it the interleaved sample count, a 6-channel stream would run six
    // times ahead of what it wrote — see `a_six_channel_loop_wraps_at_its_frame_length`.
    writer.set_file_position((file_position + written.get()) as u64);
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
    /// the loop turns into a visibly wrong frame rather than a plausible one.
    ///
    /// The cursor counts the file straight on (the loop places it as the ring
    /// is written), so 150 frames in it stands at 150, and the next refill
    /// writes the loop's frame 50 — while the ring's frame 100 is the loop's
    /// frame 0.
    ///
    /// Mutation: `push_interleaved` returning `Samples(samples.len())` (the
    /// interleaved length) → the cursor at 900, whose next frame places at 0,
    /// not 50 → the forward assertions fail. `write_interleaved_reversed`
    /// returning the sample count → 180 back from 100 saturates to 0, not 70 →
    /// the reverse assertion fails. (Re-run on the straight cursor.)
    #[test]
    fn a_six_channel_loop_wraps_at_its_frame_length() {
        use crate::butler::command::RegionId;
        use crate::butler::RegionBuffer;
        use tutti_core::Samples;

        let frame_value = |i: usize| (i * 6) as f32 / 1000.0;

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
        let ring_loop = crate::butler::loops::RingLoop::capture(
            (0, LOOP_FRAMES as u64),
            0,
            LOOP_FRAMES,
            None,
            CH,
        );
        let (mut writer, mut reader) =
            RegionBuffer::with_capacity(RegionId(1), PathBuf::new(), 4096, CH);
        refill_forward(&mut writer, &wave, 0, 150, &mut buf, ring_loop.as_ref());
        assert_eq!(writer.buffered(), Samples(150), "150 frames landed");
        assert_eq!(
            writer.file_position(),
            150,
            "150 frames in, counted straight on — a sample-denominated count would be 900"
        );
        let at = writer.file_position() as usize;
        refill_forward(&mut writer, &wave, at, 1, &mut buf, ring_loop.as_ref());
        let mut frame = [0.0f32; CH];
        let mut firsts = Vec::new();
        while reader.read_into(&mut frame) {
            firsts.push(frame[0]);
        }
        assert_eq!(firsts.len(), 151);
        assert_eq!(firsts[99], frame_value(99));
        assert_eq!(
            firsts[100],
            frame_value(0),
            "the loop wraps at its 100th frame"
        );
        assert_eq!(
            firsts[150],
            frame_value(50),
            "150 frames into a 100-frame loop is frame 50"
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

    /// **A reverse stream falls silent past the file's first frame** (doc 013
    /// follow-up S1, the butler tier): a reverse refill that reaches frame 0
    /// hands the ring frames `n - 1 … 0`, and every refill after that hands it
    /// silence. Without the silence the ring would run dry, and `DiskSource`
    /// holds its last frame through an underrun — frame 0 as DC, what the
    /// memory tier and the disk fork used to play.
    ///
    /// Mutation (run): the zero push removed from `refill_reverse`'s
    /// `actual_chunk == 0` arm → the ring holds nothing past frame 0 → fails.
    #[test]
    fn a_reverse_refill_is_silent_past_the_first_frame() {
        use crate::butler::command::RegionId;
        use crate::butler::RegionBuffer;

        let wave = make_test_wave(&[(1.0, -1.0), (2.0, -2.0), (3.0, -3.0), (4.0, -4.0)]);
        let (mut writer, mut reader) =
            RegionBuffer::with_capacity(RegionId(1), PathBuf::new(), 4096, 2usize);
        let mut buf = Vec::new();
        // From frame 4, back past the start in chunks of 3: frames 3, 2, 1;
        // then 0; then twice from the start.
        writer.set_file_position(4);
        for _ in 0..4 {
            let at = writer.file_position() as usize;
            refill_reverse(&mut writer, &wave, at, 3, &mut buf);
        }
        let mut frame = [0.0f32; 2];
        let mut left = Vec::new();
        while reader.read_into(&mut frame) {
            left.push(frame[0]);
        }
        assert_eq!(&left[..4], [4.0, 3.0, 2.0, 1.0], "the file, reversed");
        assert!(
            left.len() >= 4 + 3 && left[4..].iter().all(|&s| s == 0.0),
            "past the first frame the ring holds silence: {left:?}"
        );
    }

    #[test]
    fn test_forward_fill_via_wave_in() {
        // The forward whole-file fill now runs through WaveIn; verify the block
        // it produces matches the old fill_buffer_forward output shape.
        let wave = make_test_wave(&[(0.1, 0.1), (0.2, 0.2), (0.3, 0.3), (0.4, 0.4)]);
        let mut src = WaveIn::new(&wave, 0, 2usize);
        let mut buffer = vec![0.0f32; 3 * 2];
        src.fill_interleaved(&mut buffer);

        assert_eq!(buffer, [0.1, 0.1, 0.2, 0.2, 0.3, 0.3]);
    }

    #[test]
    fn test_forward_fill_past_end_pads_zeros() {
        let wave = make_test_wave(&[(0.1, 0.1), (0.2, 0.2)]);
        let mut src = WaveIn::new(&wave, 1, 2usize);
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
