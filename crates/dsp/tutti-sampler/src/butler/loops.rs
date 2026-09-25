//! Loop handling and crossfade capture for the butler thread.
//!
//! Loop-boundary *policy*, not I/O: it classifies where each stream stands
//! relative to its loop, arms the crossfade before the wrap, and repositions the
//! writer at it. Every position here is a file **frame**.

use super::cache::LruCache;
use super::io::refill::load_wave;
use super::io::wave_io::wave_frame_into;
use super::metrics::Metrics;
use super::plan::{ChannelPlan, LoopStatus};
use super::region_map::RegionMap;
use dashmap::DashMap;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use tutti_core::{ChannelLayout, SampleRate};
use tutti_io::Wave;

use crate::nonempty;
use crate::voice::loop_span::LoopSpan;

/// Advance every streaming channel's loop state by one butler cycle.
///
/// Approaching the loop end, capture the fadeout tail and the fadein head and
/// arm the channel's loop crossfade — the buffers are built here, on the butler
/// thread, so the audio thread only ever blends a finished pair. At the end,
/// clear the fade, flush the ring, move the writer back to the loop start and
/// prefill what fits.
///
/// The fadein comes from the pre-captured `preloop_buffer` when the loop was
/// set up with one, so a wrap re-reads nothing. Both buffers are
/// [`loop_fade_len`] frames: the tail `[end - fade, end)` and the lead-in
/// `[start - fade, start)` — the material that leads into the loop's start, so
/// the fade ends where the wrap continues (see [`capture_lead_in`]).
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
                let Some(loop_cfg) = link.loop_config() else {
                    continue;
                };
                let (loop_start, loop_end) = loop_cfg.range;
                let fade_len = loop_fade_len(loop_cfg.range, loop_cfg.crossfade_frames);
                if fade_len == 0 {
                    continue;
                }

                if let Some(writer) = regions.get(link.region_id) {
                    if let Some(wave) = load_wave(cache, metrics, writer.file_path()) {
                        let ch = writer.channels();
                        // The span on this wave: its end clamped to the file,
                        // as every tier reads it.
                        let Some(span) = LoopSpan::new(
                            loop_start as usize,
                            loop_end as usize,
                            loop_cfg.crossfade_frames,
                            wave.len(),
                        ) else {
                            continue;
                        };
                        let fade_len = span.fade();
                        let fadeout = capture_frames(&wave, span.end() - fade_len, fade_len, ch);

                        let fadein = if let Some(preloop) = loop_cfg.preloop_buffer.as_deref() {
                            preloop.to_vec()
                        } else {
                            capture_lead_in(
                                &wave,
                                (loop_start, loop_end),
                                loop_cfg.crossfade_frames,
                                ch,
                            )
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
                            .loop_config()
                            .map_or(wave.len(), |c| c.range.1 as usize);
                        let loop_len = loop_end - loop_start as usize;
                        // Both sides are FRAME counts — `write_space()` is
                        // frame-denominated, so this comparison needs no scaling.
                        let prefill_len = loop_len.min(writer.write_space().get());
                        capture_frames(&wave, loop_start as usize, prefill_len, writer.channels())
                    } else {
                        Vec::new()
                    };

                stream_state.flush_buffer();
                writer.set_file_position(loop_start);

                if !prefill_samples.is_empty() {
                    let written = writer.push_interleaved(&prefill_samples);
                    writer.set_file_position(loop_start + written.get() as u64);
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

/// The frames a loop over `range` actually fades over when `crossfade_frames`
/// are asked for, as every tier clamps it (`LoopSpan`): to the loop, and to
/// half of it when there is too little before the start for a lead-in and
/// the fade goes into the loop's head. 0 for an empty range. The file's length
/// is not known here, so the loop's end is not clamped to it.
pub(crate) fn loop_fade_len(range: (u64, u64), crossfade_frames: usize) -> usize {
    LoopSpan::new(
        range.0 as usize,
        range.1 as usize,
        crossfade_frames,
        usize::MAX,
    )
    .map_or(0, |span| span.fade())
}

/// Capture the fadein of a loop crossfade: the `fade` frames that lead into
/// where the wrap resumes, `[resume - fade, resume)` (`LoopSpan`'s rule),
/// flat interleaved at `channels`. `resume` is the loop's start, or — with
/// too little before the start for a lead-in — `start + fade`, so the fadein
/// is then the loop's own head.
///
/// The frames that lead into `resume`, not the ones from it: the fade ends on
/// frame `resume - 1` and the wrap continues at `resume`, so the join is the
/// file's own step. Capturing `[start, start + fade)` and wrapping to `start`
/// — what this did first — faded into the loop's head and then played the
/// head again after the wrap: a jump of `fade` frames at every loop.
pub(crate) fn capture_lead_in(
    wave: &Wave,
    range: (u64, u64),
    fade: usize,
    channels: impl Into<ChannelLayout>,
) -> Vec<f32> {
    match LoopSpan::new(range.0 as usize, range.1 as usize, fade, wave.len()) {
        Some(span) => capture_frames(wave, span.resume() - span.fade(), span.fade(), channels),
        None => Vec::new(),
    }
}

/// Capture the fadeout buffer for a seek crossfade — the `count` **frames**
/// about to play next, i.e. the ring's unplayed head.
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
    file_path: &Path,
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

/// Capture `count` **frames** from `file_path` starting at frame
/// `position_samples`, flat interleaved at `channels` samples per frame — the
/// fadein head for a seek or loop crossfade.
///
/// Despite its name `position_samples` is a **frame** offset: it is compared
/// against and derived from `read_position` and `file_position`, both of which
/// count frames.
///
/// Empty when `count` is zero or the file cannot be loaded; both callers treat
/// an empty buffer as "skip the crossfade", which degrades to a hard cut rather
/// than to silence.
pub(crate) fn fadein_samples(
    cache: &LruCache,
    metrics: &Metrics,
    file_path: &Path,
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

/// Ring capacity in **frames** for a file of `file_length_samples` frames at
/// `sample_rate`.
///
/// Buys buffering depth in seconds of audio, tapering as the file grows: a small
/// file is held whole up to a 30 s cap, then 10 s, 5 s and 3 s as the estimated
/// size crosses 50 MB, 200 MB and 500 MB. Floored at 4096 frames, which is what
/// keeps a very short file from producing a ring too small to absorb one block.
///
/// The size estimate assumes stereo `f32`. At a wider width it under-estimates,
/// which only picks a slightly more generous buffer — the heuristic chooses a
/// capacity, never a correctness boundary.
pub(crate) fn buffer_size_for_file(
    file_length_samples: u64,
    sample_rate: impl Into<SampleRate>,
) -> usize {
    let sample_rate = sample_rate.into().get();
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
            wave.push_frame(&[*l, *r]);
        }
        wave
    }

    fn make_mono_wave(samples: &[f32]) -> Wave {
        let mut wave = Wave::new(1, 48000.0);
        for s in samples {
            wave.push_frame(&[*s]);
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

    /// **The loop's fadein is what leads into where the wrap resumes**:
    /// `[start - fade, start)` when there is room before the start, else the
    /// loop's own head `[start, start + fade)` (the wrap then resuming after
    /// it), the fade at most half the loop there — `LoopSpan`'s rule, which
    /// every tier reads by.
    ///
    /// Mutation (run): `capture_lead_in` capturing from `start` in every mode
    /// (the old head replay) → `[5.0, 6.0]` → fails. Mutation (run): the head
    /// mode removed from `LoopSpan::new` (the fade clamped to `start`) → a
    /// 2-frame fade before frame 2 → fails.
    #[test]
    fn a_loop_fades_in_from_what_leads_into_its_resume() {
        let wave = make_mono_wave(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0]);
        assert_eq!(capture_lead_in(&wave, (5, 9), 2, 1usize), [3.0, 4.0]);
        assert_eq!(loop_fade_len((5, 7), 4), 2);
        assert_eq!(loop_fade_len((2, 9), 4), 3);
        assert_eq!(capture_lead_in(&wave, (2, 9), 4, 1usize), [2.0, 3.0, 4.0]);
        assert_eq!(loop_fade_len((0, 9), 4), 4);
        assert_eq!(
            capture_lead_in(&wave, (0, 9), 4, 1usize),
            [0.0, 1.0, 2.0, 3.0]
        );
        assert!(capture_lead_in(&wave, (0, 9), 0, 1usize).is_empty());
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
