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
use tutti_core::Wave;

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
#[allow(clippy::too_many_arguments)]
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

        let available = writer.capacity() - writer.write_space();
        let buffer_capacity = writer.capacity();

        let fill_pct = available as f32 / buffer_capacity as f32;

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

            let available = writer.capacity() - writer.write_space();
            let fill_pct = available as f32 / writer.capacity() as f32;

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
#[allow(clippy::too_many_arguments)]
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

    let new_pos = wrap_position(file_position + written, loop_range);
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
    writer.set_file_position(file_position.saturating_sub(written) as u64);
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

    let new_pos = wrap_position(file_position + written, loop_range);
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
    writer.set_file_position(file_position.saturating_sub(written) as u64);
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

    // `Wave::load` lives in fundsp's `read` module, which is compiled only when
    // a codec feature is on — calling it unconditionally breaks the
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

    #[test]
    fn test_varifill_empty_buffer_increases_chunk() {
        // Empty buffer (fill=0) should have high urgency -> larger chunk
        let base = 4096;
        let chunk = varifill_chunk(0.0, base, 10_000_000.0, 1.0);

        // urgency=1.0, multiplier = (0.5 + 1.0*1.5) * 1.0 * 1.0 = 2.0
        assert!(chunk > base, "Empty buffer should increase chunk size");
        assert_eq!(chunk, base * 2);
    }

    #[test]
    fn test_varifill_full_buffer_decreases_chunk() {
        // Full buffer (fill=1) should have low urgency -> smaller chunk
        let base = 4096;
        let chunk = varifill_chunk(1.0, base, 10_000_000.0, 1.0);

        // urgency=0.0, multiplier = (0.5 + 0.0*1.5) * 1.0 * 1.0 = 0.5
        assert!(chunk < base, "Full buffer should decrease chunk size");
        assert_eq!(chunk, base / 2);
    }

    #[test]
    fn test_varifill_half_buffer_near_base() {
        // Half-full buffer should be close to base chunk
        let base = 4096;
        let chunk = varifill_chunk(0.5, base, 10_000_000.0, 1.0);

        // urgency=0.5, multiplier = (0.5 + 0.5*1.5) * 1.0 * 1.0 = 1.25
        assert_eq!(chunk, (base as f64 * 1.25) as usize);
    }

    #[test]
    fn test_varifill_high_speed_increases_chunk() {
        // High playback speed should increase chunk to keep up
        let base = 4096;
        let normal = varifill_chunk(0.5, base, 10_000_000.0, 1.0);
        let fast = varifill_chunk(0.5, base, 10_000_000.0, 2.0);

        assert!(fast > normal, "Higher speed should increase chunk");
        assert_eq!(fast, normal * 2);
    }

    #[test]
    fn test_varifill_slow_speed_no_decrease() {
        // Slow speed (<1.0) should NOT decrease chunk (use max(1.0))
        let base = 4096;
        let normal = varifill_chunk(0.5, base, 10_000_000.0, 1.0);
        let slow = varifill_chunk(0.5, base, 10_000_000.0, 0.5);

        assert_eq!(
            slow, normal,
            "Slow speed should not decrease chunk below normal"
        );
    }

    #[test]
    fn test_varifill_high_bandwidth_increases_chunk() {
        // High disk throughput allows larger chunks
        let base = 4096;
        let normal = varifill_chunk(0.5, base, 10_000_000.0, 1.0);
        let fast_disk = varifill_chunk(0.5, base, 40_000_000.0, 1.0); // 4x bandwidth

        // bandwidth_factor = sqrt(4) = 2.0
        assert!(
            fast_disk > normal,
            "Higher bandwidth should allow larger chunks"
        );
    }

    #[test]
    fn test_varifill_low_bandwidth_decreases_chunk() {
        // Low disk throughput should use smaller chunks
        let base = 4096;
        let normal = varifill_chunk(0.5, base, 10_000_000.0, 1.0);
        let slow_disk = varifill_chunk(0.5, base, 2_500_000.0, 1.0); // 0.25x bandwidth

        // bandwidth_factor = sqrt(0.25) = 0.5
        assert!(
            slow_disk < normal,
            "Lower bandwidth should use smaller chunks"
        );
    }

    #[test]
    fn test_varifill_zero_bandwidth_uses_default() {
        // Zero bandwidth should use factor of 1.0
        let base = 4096;
        let normal = varifill_chunk(0.5, base, 10_000_000.0, 1.0);
        let zero_bw = varifill_chunk(0.5, base, 0.0, 1.0);

        assert_eq!(zero_bw, normal, "Zero bandwidth should use default factor");
    }

    #[test]
    fn test_varifill_minimum_chunk_size() {
        // Even with everything minimal, chunk should be at least 1024
        let chunk = varifill_chunk(1.0, 100, 1_000_000.0, 1.0);

        assert!(chunk >= 1024, "Minimum chunk size should be 1024");
    }

    #[test]
    fn test_varifill_clamps_multiplier() {
        // Extreme values should be clamped
        let base = 4096;

        // Very empty buffer + fast disk + high speed
        let extreme_high = varifill_chunk(0.0, base, 100_000_000.0, 4.0);
        // Multiplier would be (0.5 + 1.5) * 2.0 * 4.0 = 16.0, clamped to 4.0
        assert_eq!(extreme_high, base * 4, "Should clamp to 4x base");

        // Very full buffer + slow disk
        let extreme_low = varifill_chunk(1.0, base, 1_000_000.0, 1.0);
        // Multiplier would be 0.5 * 0.316 * 1.0 = 0.158, clamped to 0.25
        // But then max(1024) kicks in
        assert!(extreme_low >= 1024, "Should respect minimum chunk size");
    }

    #[test]
    fn test_varifill_bandwidth_factor_clamped() {
        // Bandwidth factor should be clamped between 0.5 and 2.0
        let base = 4096;

        // Very high bandwidth (100x baseline)
        let very_high = varifill_chunk(0.5, base, 1_000_000_000.0, 1.0);
        // sqrt(100) = 10, but clamped to 2.0
        let expected_high = (base as f64 * 1.25 * 2.0) as usize;
        assert_eq!(very_high, expected_high);

        // Very low bandwidth (0.01x baseline)
        let very_low = varifill_chunk(0.5, base, 100_000.0, 1.0);
        // sqrt(0.01) = 0.1, but clamped to 0.5
        let expected_low = (base as f64 * 1.25 * 0.5) as usize;
        assert_eq!(very_low, expected_low);
    }

    #[test]
    fn test_varifill_buffer_fill_over_one() {
        // Buffer fill > 1.0 (shouldn't happen, but test robustness)
        let chunk = varifill_chunk(1.5, 4096, 10_000_000.0, 1.0);

        // urgency = 1.0 - 1.5 = -0.5
        // multiplier = (0.5 + (-0.5)*1.5) * 1.0 * 1.0 = -0.25, clamped to 0.25
        assert!(chunk >= 1024, "Should still respect minimum");
    }

    #[test]
    fn test_varifill_buffer_fill_negative() {
        // Buffer fill < 0 (shouldn't happen, but test robustness)
        let chunk = varifill_chunk(-0.5, 4096, 10_000_000.0, 1.0);

        // urgency = 1.0 - (-0.5) = 1.5
        // multiplier = (0.5 + 1.5*1.5) * 1.0 * 1.0 = 2.75
        assert!(chunk > 4096, "Should increase chunk for negative fill");
    }

    #[test]
    fn test_varifill_nan_buffer_fill() {
        // NaN buffer_fill - should not crash, clamp handles it
        let chunk = varifill_chunk(f32::NAN, 4096, 10_000_000.0, 1.0);
        assert!(chunk >= 1024, "Should respect minimum even with NaN");
    }

    #[test]
    fn test_varifill_infinity_bandwidth() {
        // Infinite bandwidth - should be clamped
        let chunk = varifill_chunk(0.5, 4096, f64::INFINITY, 1.0);
        // sqrt(inf) = inf, but clamped to 2.0
        let expected = (4096.0 * 1.25 * 2.0) as usize;
        assert_eq!(chunk, expected);
    }

    #[test]
    fn test_varifill_negative_bandwidth() {
        // Negative bandwidth (invalid) - should use default factor 1.0
        let chunk = varifill_chunk(0.5, 4096, -1000.0, 1.0);
        let normal = varifill_chunk(0.5, 4096, 10_000_000.0, 1.0);
        // Negative is not > 0, so uses factor 1.0
        assert_eq!(chunk, normal);
    }

    fn make_test_wave(samples: &[(f32, f32)]) -> Wave {
        let mut wave = Wave::new(2, 48000.0);
        for (l, r) in samples {
            wave.push((*l, *r));
        }
        wave
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
