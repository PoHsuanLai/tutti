//! Loop handling and crossfade capture for butler thread.

use super::cache::LruCache;
use super::io::refill::load_wave;
use super::io::wave_io::wave_frame_into;
use super::metrics::Metrics;
use super::plan::{ChannelPlan, LoopStatus};
use super::region_map::RegionMap;
use dashmap::DashMap;
use std::path::PathBuf;
use tutti_core::{ChannelLayout, Wave};

use crate::nonempty;

/// Check and handle stream loop conditions with crossfade support.
///
/// Loop crossfade is now handled via RtState for lock-free audio thread access.
/// Butler captures fadeout/fadein samples and passes them to RtState.
pub(crate) fn handle_loops(
    plans: &DashMap<usize, ChannelPlan>,
    regions: &mut RegionMap,
    cache: &LruCache,
    metrics: &Metrics,
) {
    for stream_entry in plans.iter() {
        let stream_state = stream_entry.value();
        let loop_status = stream_state.check_loop_status();

        match loop_status {
            LoopStatus::Normal => continue,
            LoopStatus::ApproachingEnd => {
                if stream_state.rt_state.is_loop_crossfading() {
                    continue;
                }

                let Some(link) = stream_state.link.as_ref() else {
                    continue;
                };
                let Some(loop_cfg) = link.loop_config.as_ref() else {
                    continue;
                };
                let fade_len = loop_cfg.crossfade_samples;
                if fade_len == 0 {
                    continue;
                }

                let (loop_start, loop_end) = loop_cfg.range;

                if let Some(writer) = regions.get(link.region_id) {
                    if let Some(wave) = load_wave(cache, metrics, writer.file_path()) {
                        let ch = writer.channels();
                        let fadeout_start = (loop_end as usize).saturating_sub(fade_len);
                        let fadeout = capture_frames(&wave, fadeout_start, fade_len, ch);

                        let fadein = if let Some(preloop) = loop_cfg.preloop_buffer.as_deref() {
                            preloop.to_vec()
                        } else {
                            capture_frames(&wave, loop_start as usize, fade_len, ch)
                        };

                        stream_state
                            .rt_state
                            .start_loop_crossfade(fadeout, fadein, ch);
                    }
                }
            }
            LoopStatus::AtEnd(loop_start) => {
                stream_state.rt_state.clear_loop_crossfade();

                let Some(link) = stream_state.link.as_ref() else {
                    continue;
                };
                let Some(writer) = regions.get_mut(link.region_id) else {
                    continue;
                };

                let prefill_samples =
                    if let Some(wave) = load_wave(cache, metrics, writer.file_path()) {
                        let loop_end = link
                            .loop_config
                            .as_ref()
                            .map_or(wave.len(), |c| c.range.1 as usize);
                        let loop_len = loop_end - loop_start as usize;
                        // Both sides are FRAME counts — `write_space()` is
                        // frame-denominated, so this comparison needs no scaling.
                        let prefill_len = loop_len.min(writer.write_space());
                        capture_frames(&wave, loop_start as usize, prefill_len, writer.channels())
                    } else {
                        Vec::new()
                    };

                stream_state.flush_buffer();
                writer.set_file_position(loop_start);

                if !prefill_samples.is_empty() {
                    let written = writer.push_interleaved(&prefill_samples);
                    writer.set_file_position(loop_start + written as u64);
                }
            }
        }
    }
}

/// Capture `count` frames from a wave file as a flat interleaved buffer at
/// `channels` samples per frame, for a crossfade.
///
/// Butler thread only, so the allocation is fine — the whole lock-free
/// crossfade design is built around handing the RT side a finished buffer.
pub(crate) fn capture_frames(
    wave: &Wave,
    start: usize,
    count: usize,
    channels: impl Into<ChannelLayout>,
) -> Vec<f32> {
    // Stride derived once, above the frame loop. Butler thread, not RT.
    let ch = nonempty(channels.into()).count() as usize;
    let mut samples = vec![0.0f32; count * ch];
    for (i, frame) in samples.chunks_exact_mut(ch).enumerate() {
        wave_frame_into(wave, start + i, frame);
    }
    samples
}

/// Capture the fadeout samples for a seek crossfade — the `count` samples about
/// to play next, i.e. the ring's unplayed head.
///
/// Sourced from the wave file (via the cache) at the stream's current
/// `read_position` rather than by popping the SPSC ring. The butler must never
/// pop the consumer — that is the audio thread's sole province (see
/// [`ReaderCell`](crate::butler::prefetch::ReaderCell)'s single-consumer
/// invariant). The ring head at `read_position` is bit-identical to
/// `file[read_position..]`, so reading from the file yields the same fadeout
/// tail without touching the consumer.
pub(crate) fn fadeout_samples(
    stream_state: &ChannelPlan,
    cache: &LruCache,
    metrics: &Metrics,
    file_path: &PathBuf,
    count: usize,
    channels: impl Into<ChannelLayout>,
) -> Vec<f32> {
    if count == 0 {
        return Vec::new();
    }

    let Some(link) = stream_state.link.as_ref() else {
        return Vec::new();
    };

    let read_position = link.read_position.load(tutti_core::Ordering::Relaxed);

    // Same file-sourced capture as `fadein_samples`, anchored at the position
    // the ring is about to hand to the audio thread.
    fadein_samples(cache, metrics, file_path, read_position, count, channels)
}

/// Capture samples from the Wave file at the new seek position for fadein.
pub(crate) fn fadein_samples(
    cache: &LruCache,
    metrics: &Metrics,
    file_path: &PathBuf,
    position_samples: u64,
    count: usize,
    channels: impl Into<ChannelLayout>,
) -> Vec<f32> {
    if count == 0 {
        return Vec::new();
    }

    let Some(wave) = load_wave(cache, metrics, file_path) else {
        return Vec::new();
    };

    capture_frames(&wave, position_samples as usize, count, channels)
}

pub(crate) fn buffer_size_for_file(file_length_samples: u64, sample_rate: f64) -> usize {
    // Rough byte estimate for the buffer-size heuristic. Assumes stereo f32;
    // at a wider width it under-estimates, which only makes the chosen buffer
    // slightly generous — it never affects correctness.
    let file_size_bytes = file_length_samples * 2 * 4;
    let file_size_mb = file_size_bytes as f64 / (1024.0 * 1024.0);

    let buffer_seconds = if file_size_mb < 50.0 {
        (file_length_samples as f64 / sample_rate).min(30.0)
    } else if file_size_mb < 200.0 {
        10.0
    } else if file_size_mb < 500.0 {
        5.0
    } else {
        3.0
    };

    let buffer_capacity = (buffer_seconds * sample_rate) as usize;
    buffer_capacity.max(4096)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_wave(samples: &[(f32, f32)]) -> Wave {
        let mut wave = Wave::new(2, 48000.0);
        for (l, r) in samples {
            wave.push((*l, *r));
        }
        wave
    }

    fn make_mono_wave(samples: &[f32]) -> Wave {
        let mut wave = Wave::new(1, 48000.0);
        for s in samples {
            wave.push(*s);
        }
        wave
    }

    #[test]
    fn test_capture_samples_basic() {
        let wave = make_test_wave(&[(1.0, 2.0), (3.0, 4.0), (5.0, 6.0), (7.0, 8.0)]);

        let captured = capture_frames(&wave, 0, 3, 2usize);

        assert_eq!(captured.len(), 3 * 2, "3 frames x 2 channels");
        assert_eq!(captured, [1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    }

    #[test]
    fn test_capture_samples_with_offset() {
        let wave = make_test_wave(&[(1.0, 2.0), (3.0, 4.0), (5.0, 6.0), (7.0, 8.0)]);

        let captured = capture_frames(&wave, 2, 2, 2usize);

        assert_eq!(captured.len(), 2 * 2);
        assert_eq!(captured, [5.0, 6.0, 7.0, 8.0]);
    }

    #[test]
    fn test_capture_samples_past_end_pads_zeros() {
        let wave = make_test_wave(&[(1.0, 2.0), (3.0, 4.0)]);

        let captured = capture_frames(&wave, 1, 4, 2usize);

        assert_eq!(captured.len(), 4 * 2);
        assert_eq!(&captured[0..2], [3.0, 4.0]); // Valid sample
        assert!(
            captured[2..].iter().all(|&s| s == 0.0),
            "past end must be zero"
        );
    }

    #[test]
    fn test_capture_samples_empty_request() {
        let wave = make_test_wave(&[(1.0, 2.0)]);

        let captured = capture_frames(&wave, 0, 0, 2usize);

        assert!(captured.is_empty());
    }

    #[test]
    fn test_capture_samples_mono_duplicates_to_stereo() {
        let wave = make_mono_wave(&[1.0, 2.0, 3.0]);

        let captured = capture_frames(&wave, 0, 3, 2usize);

        assert_eq!(captured.len(), 3 * 2, "3 frames x 2 channels");
        // Mono fans to every channel — the shared policy, same as the RT reader.
        assert_eq!(&captured[0..2], [1.0, 1.0]);
        assert_eq!(&captured[2..4], [2.0, 2.0]);
        assert_eq!(&captured[4..6], [3.0, 3.0]);
    }

    #[test]
    fn test_buffer_size_small_file() {
        // Small file: 1 second at 48kHz = 48000 samples
        // file_size_bytes = 48000 * 2 * 4 = 384000 bytes = 0.37 MB
        // buffer_seconds = min(1.0, 30.0) = 1.0
        let size = buffer_size_for_file(48000, 48000.0);
        assert_eq!(size, 48000); // 1 second buffer
    }

    #[test]
    fn test_buffer_size_medium_file() {
        // Medium file: 100MB = 100 * 1024 * 1024 bytes
        // file_size_bytes = file_length * 2 * 4 = file_length * 8
        // For 100MB: file_length = 100 * 1024 * 1024 / 8 = 13,107,200 samples
        let file_length = 100 * 1024 * 1024 / 8;
        let size = buffer_size_for_file(file_length, 48000.0);

        // 100MB is in 50-200MB range, so buffer_seconds = 10.0
        let expected = (10.0 * 48000.0) as usize;
        assert_eq!(size, expected);
    }

    #[test]
    fn test_buffer_size_large_file() {
        // Large file: 300MB
        let file_length = 300 * 1024 * 1024 / 8;
        let size = buffer_size_for_file(file_length, 48000.0);

        // 300MB is in 200-500MB range, so buffer_seconds = 5.0
        let expected = (5.0 * 48000.0) as usize;
        assert_eq!(size, expected);
    }

    #[test]
    fn test_buffer_size_very_large_file() {
        // Very large file: 1GB
        let file_length = 1024 * 1024 * 1024 / 8;
        let size = buffer_size_for_file(file_length, 48000.0);

        // 1GB > 500MB, so buffer_seconds = 3.0
        let expected = (3.0 * 48000.0) as usize;
        assert_eq!(size, expected);
    }

    #[test]
    fn test_buffer_size_minimum() {
        // Tiny file should still have minimum buffer
        let size = buffer_size_for_file(100, 48000.0);
        assert!(size >= 4096, "Buffer should be at least 4096 samples");
    }

    #[test]
    fn test_buffer_size_small_file_capped_at_30s() {
        // File that would need more than 30 seconds should be capped
        // 60 seconds at 48kHz = 2,880,000 samples
        // file_size = 2,880,000 * 8 = 23MB (< 50MB, so uses file duration)
        // But capped at 30 seconds
        let file_length = 60 * 48000; // 60 seconds
        let size = buffer_size_for_file(file_length, 48000.0);

        let expected = (30.0 * 48000.0) as usize; // Capped at 30s
        assert_eq!(size, expected);
    }

    #[test]
    fn test_capture_fadeout_zero_count() {
        use crate::butler::plan::ChannelPlan;

        let cache = LruCache::new(10, 1024 * 1024);
        let metrics = Metrics::new();
        let path = PathBuf::from("nonexistent.wav");
        let state = ChannelPlan::default();
        let samples = fadeout_samples(&state, &cache, &metrics, &path, 0, 2usize);

        assert!(samples.is_empty());
    }

    #[test]
    fn test_capture_fadeout_no_consumer() {
        use crate::butler::plan::ChannelPlan;

        let cache = LruCache::new(10, 1024 * 1024);
        let metrics = Metrics::new();
        let path = PathBuf::from("nonexistent.wav");
        let state = ChannelPlan::default();
        // No active link → nothing to fade out.
        let samples = fadeout_samples(&state, &cache, &metrics, &path, 100, 2usize);

        assert!(samples.is_empty());
    }

    #[test]
    fn test_capture_fadein_zero_count() {
        let cache = LruCache::new(10, 1024 * 1024);
        let metrics = Metrics::new();
        let path = PathBuf::from("nonexistent.wav");

        let samples = fadein_samples(&cache, &metrics, &path, 0, 0, 2usize);

        assert!(samples.is_empty());
    }

    #[test]
    fn test_capture_fadein_file_not_in_cache() {
        let cache = LruCache::new(10, 1024 * 1024);
        let metrics = Metrics::new();
        let path = PathBuf::from("nonexistent.wav");

        let samples = fadein_samples(&cache, &metrics, &path, 0, 100, 2usize);

        // File not in cache and doesn't exist, so returns empty
        assert!(samples.is_empty());
    }

    #[test]
    fn test_capture_samples_empty_wave() {
        // Empty wave with 0 samples - should return zeros
        let wave = Wave::new(2, 48000.0);
        assert_eq!(wave.len(), 0);

        let captured = capture_frames(&wave, 0, 3, 2usize);

        // Should pad with zeros since wave is empty
        assert_eq!(captured.len(), 3 * 2, "3 frames x 2 channels");
        assert_eq!(&captured[0..2], [0.0, 0.0]);
        assert_eq!(&captured[2..4], [0.0, 0.0]);
        assert_eq!(&captured[4..6], [0.0, 0.0]);
    }

    #[test]
    fn test_capture_samples_large_start_no_panic() {
        let wave = make_test_wave(&[(1.0, 2.0), (3.0, 4.0)]);

        // Start way past wave length - should not panic, just return zeros
        let captured = capture_frames(&wave, 1_000_000, 5, 2usize);

        // All indices are way past wave.len(), so all zeros
        assert_eq!(captured.len(), 5 * 2, "5 frames x 2 channels");
        assert!(captured.iter().all(|&s| s == 0.0));
    }
}
