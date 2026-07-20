//! Ring buffer refill logic for butler thread.

use super::super::cache::LruCache;
use super::super::metrics::Metrics;
use super::super::plan::ChannelPlan;
use super::super::prefetch::RegionWriter;
use super::super::region_map::RegionMap;
use dashmap::DashMap;
use rayon::prelude::*;
use std::path::PathBuf;
use std::sync::Arc;
use tutti_core::Wave;

/// Calculate optimal chunk size using varifill strategy.
///
/// Adapts chunk size based on:
/// - Buffer urgency (how empty the buffer is)
/// - Disk bandwidth (recent read throughput)
/// - Playback speed (varispeed)
///
/// Returns chunk size in samples.
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

/// Refill ring buffers from disk with varifill strategy.
///
/// Uses a pre-allocated buffer to avoid allocation in the hot path.
/// Loop crossfade is handled by the audio thread via RtState.
///
/// Chunk size is dynamically adjusted (varifill) based on:
/// - Buffer urgency (how empty the buffer is)
/// - Disk throughput (recent read rate)
/// - Playback speed (varispeed)
#[allow(clippy::too_many_arguments)]
pub(crate) fn refill_all(
    plans: &DashMap<usize, ChannelPlan>,
    regions: &mut RegionMap,
    cache: &LruCache,
    metrics: &Metrics,
    base_chunk_size: usize,
    buffer_margin: f64,
    interleave_buffer: &mut Vec<(f32, f32)>,
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
        let speed = stream_state.rt_state.effective_speed().get();
        let src_ratio = stream_state.rt_state.src_ratio().get();

        let adjusted_speed = speed * src_ratio * buffer_margin as f32;
        let chunk_size = varifill_chunk(fill_pct, base_chunk_size, read_rate, adjusted_speed);

        let file_path = writer.file_path();
        let Some(wave) = load_wave(cache, metrics, file_path) else {
            continue;
        };

        let file_position = writer.file_position() as usize;
        let channels = wave.channels();

        let loop_range = stream_state.loop_config().map(|c| c.range);

        if is_reverse {
            refill_reverse(
                writer,
                &wave,
                file_position,
                chunk_size,
                channels,
                interleave_buffer,
            );
        } else {
            refill_forward(
                writer,
                &wave,
                file_position,
                chunk_size,
                channels,
                interleave_buffer,
                loop_range,
            );
        }
    }
}

struct RefillWorkItem {
    writer_idx: usize,
    chunk_size: usize,
    is_reverse: bool,
    file_path: PathBuf,
    fill_pct: f32,
    shared: Arc<super::super::rt_state::RtState>,
}

/// Parallel refill using rayon's par_iter_mut with varifill strategy.
///
/// Uses Vec<RegionWriter> with par_iter_mut which only requires Send, not Sync.
/// Each rayon worker gets exclusive &mut access to a different producer.
/// Only used when parallel_io is enabled and there are 3+ streams.
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

            let speed = stream_state.rt_state.effective_speed().get();
            let src_ratio = stream_state.rt_state.src_ratio().get();

            let adjusted_speed = speed * src_ratio * buffer_margin as f32;
            let chunk_size = varifill_chunk(fill_pct, base_chunk_size, read_rate, adjusted_speed);

            let shared = stream_state.rt_state();

            Some(RefillWorkItem {
                writer_idx: idx,
                chunk_size,
                is_reverse: stream_state.rt_state.is_reverse(),
                file_path: writer.file_path().to_path_buf(),
                fill_pct,
                shared,
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
                static LOCAL_BUF: std::cell::RefCell<Vec<(f32, f32)>> =
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
                );
            });
        });
}

/// Refill a single stream (used by parallel path). Takes a direct mutable
/// reference to the writer from `par_iter_mut`.
#[allow(clippy::too_many_arguments)]
fn refill_one(
    writer: &mut RegionWriter,
    cache: &LruCache,
    chunk_size: usize,
    is_reverse: bool,
    file_path: &PathBuf,
    fill_pct: f32,
    shared: &super::super::rt_state::RtState,
    buffer: &mut Vec<(f32, f32)>,
) {
    let Some(wave) = cache.get(file_path) else {
        return;
    };

    shared.set_buffer_fill(fill_pct);

    let file_position = writer.file_position() as usize;
    let channels = wave.channels();

    buffer.clear();

    if is_reverse {
        fill_buffer_reverse(&wave, file_position, chunk_size, channels, buffer);
        let written = writer.write(buffer);
        writer.set_file_position(file_position.saturating_sub(written) as u64);
    } else {
        fill_buffer_forward(&wave, file_position, chunk_size, channels, buffer);
        let written = writer.write(buffer);
        writer.set_file_position((file_position + written) as u64);
    }
}

/// Fill buffer with forward samples (no ring buffer write).
#[inline]
fn fill_buffer_forward(
    wave: &Wave,
    file_position: usize,
    chunk_size: usize,
    channels: usize,
    buffer: &mut Vec<(f32, f32)>,
) {
    for i in 0..chunk_size {
        let sample_idx = file_position + i;
        let sample = if sample_idx >= wave.len() {
            (0.0, 0.0)
        } else {
            let left = wave.at(0, sample_idx);
            let right = if channels > 1 {
                wave.at(1, sample_idx)
            } else {
                left
            };
            (left, right)
        };
        buffer.push(sample);
    }
}

/// Fill buffer with reversed samples (no ring buffer write).
#[inline]
fn fill_buffer_reverse(
    wave: &Wave,
    file_position: usize,
    chunk_size: usize,
    channels: usize,
    buffer: &mut Vec<(f32, f32)>,
) {
    let read_start = file_position.saturating_sub(chunk_size);
    let actual_chunk = file_position - read_start;

    if actual_chunk == 0 {
        for _ in 0..chunk_size {
            buffer.push((0.0, 0.0));
        }
        return;
    }

    let temp: Vec<(f32, f32)> = (0..actual_chunk)
        .map(|i| {
            let sample_idx = read_start + i;
            let left = wave.at(0, sample_idx);
            let right = if channels > 1 {
                wave.at(1, sample_idx)
            } else {
                left
            };
            (left, right)
        })
        .collect();

    for sample in temp.into_iter().rev() {
        buffer.push(sample);
    }
}

/// Refill buffer for forward playback, respecting loop boundaries if set.
#[allow(clippy::too_many_arguments)]
fn refill_forward(
    writer: &mut RegionWriter,
    wave: &Wave,
    file_position: usize,
    chunk_size: usize,
    channels: usize,
    interleave_buffer: &mut Vec<(f32, f32)>,
    loop_range: Option<(u64, u64)>,
) {
    interleave_buffer.clear();

    let loop_bounds = loop_range.and_then(|(start, end)| {
        let start = start as usize;
        let end = end as usize;
        let len = end.saturating_sub(start);
        (len > 0).then_some((start, end, len))
    });

    let mut pos = file_position;

    for _ in 0..chunk_size {
        if let Some((loop_start, loop_end, loop_len)) = loop_bounds {
            if pos >= loop_end {
                pos = loop_start + ((pos - loop_start) % loop_len);
            }
        }

        let sample = if pos >= wave.len() {
            (0.0, 0.0)
        } else {
            let left = wave.at(0, pos);
            let right = if channels > 1 { wave.at(1, pos) } else { left };
            (left, right)
        };

        interleave_buffer.push(sample);
        pos += 1;
    }

    let written = writer.write(interleave_buffer);

    let mut new_pos = file_position + written;
    if let Some((loop_start, loop_end, loop_len)) = loop_bounds {
        if new_pos >= loop_end {
            new_pos = loop_start + ((new_pos - loop_start) % loop_len);
        }
    }
    writer.set_file_position(new_pos as u64);
}

/// Refill buffer for reverse playback.
/// Reads samples forward from disk, then writes them reversed to the ring buffer.
fn refill_reverse(
    writer: &mut RegionWriter,
    wave: &Wave,
    file_position: usize,
    chunk_size: usize,
    channels: usize,
    interleave_buffer: &mut Vec<(f32, f32)>,
) {
    let read_start = file_position.saturating_sub(chunk_size);
    let actual_chunk = file_position - read_start;

    if actual_chunk == 0 {
        interleave_buffer.clear();
        for _ in 0..chunk_size {
            interleave_buffer.push((0.0, 0.0));
        }
        writer.write(interleave_buffer);
        return;
    }

    interleave_buffer.clear();
    for i in 0..actual_chunk {
        let sample_idx = read_start + i;
        let left = wave.at(0, sample_idx);
        let right = if channels > 1 {
            wave.at(1, sample_idx)
        } else {
            left
        };
        interleave_buffer.push((left, right));
    }

    let written = writer.write_reversed(interleave_buffer);
    writer.set_file_position(file_position.saturating_sub(written) as u64);
}

pub(in crate::butler) fn load_wave(
    cache: &LruCache,
    metrics: &Metrics,
    file_path: &PathBuf,
) -> Option<Arc<Wave>> {
    if let Some(cached) = cache.get(file_path) {
        metrics.record_cache_hit();
        Some(cached)
    } else {
        metrics.record_cache_miss();
        match Wave::load(file_path) {
            Ok(w) => {
                let arc_wave = Arc::new(w);
                let bytes = arc_wave.len() as u64 * arc_wave.channels() as u64 * 4;
                metrics.record_read(bytes);
                cache.insert(file_path.clone(), arc_wave.clone());
                Some(arc_wave)
            }
            Err(_) => None,
        }
    }
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
    fn test_fill_buffer_forward_basic() {
        // Create a simple wave: 0.1, 0.2, 0.3, 0.4
        let wave = make_test_wave(&[(0.1, 0.1), (0.2, 0.2), (0.3, 0.3), (0.4, 0.4)]);
        let mut buffer = Vec::new();

        fill_buffer_forward(&wave, 0, 3, 2, &mut buffer);

        assert_eq!(buffer.len(), 3);
        assert_eq!(buffer[0], (0.1, 0.1));
        assert_eq!(buffer[1], (0.2, 0.2));
        assert_eq!(buffer[2], (0.3, 0.3));
    }

    #[test]
    fn test_fill_buffer_forward_past_end_pads_zeros() {
        let wave = make_test_wave(&[(0.1, 0.1), (0.2, 0.2)]);
        let mut buffer = Vec::new();

        fill_buffer_forward(&wave, 1, 4, 2, &mut buffer);

        assert_eq!(buffer.len(), 4);
        assert_eq!(buffer[0], (0.2, 0.2)); // Last valid sample
        assert_eq!(buffer[1], (0.0, 0.0)); // Past end - zeros
        assert_eq!(buffer[2], (0.0, 0.0));
        assert_eq!(buffer[3], (0.0, 0.0));
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
