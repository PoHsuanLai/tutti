//! A looped stream, as the butler writes it into the ring.
//!
//! The ring carries a looped stream as the continuous sequence of frames
//! [`LoopSpan`] defines, the one the memory tier and a forked disk voice read:
//! the file straight on up to the loop's end, the fade's frames blended toward
//! their lead-in as they are written, then `[resume, end)` again, forever. The
//! audio thread does no loop logic at all. It consumes the ring, so what it
//! plays through a wrap is whatever the butler wrote, and the butler writes the
//! sequence.
//!
//! # A stream's position counts the file straight on
//!
//! The writer's cursor ([`RegionOut::file_position`]) counts file frames as if
//! the file played straight on, never wrapped; a loop *places* it
//! ([`LoopSpan::place_frame`]) as each run is written ([`fill_sequence`]).
//! Straight rather than placed because every move of a stream is a move along
//! that line: the placement gate's seek target is a straight offset into the
//! clip (`DiskVoice::maybe_seek`), a PDC delta moves it by frames of the
//! sequence, and a changed loop re-places the same position, as the memory
//! tier re-places the position its clock gives
//! (`MemorySource::read_placed_into`). A reverse stream ignores the loop, on
//! every tier, so its position is a file frame (`io::refill`'s module docs).
//!
//! # What this replaced
//!
//! The butler used to classify each stream against its loop by the reader's
//! `read_position` (frames *consumed*, plus every flush) as if it were a file
//! frame, flush the ring at the loop's end and arm an RT crossfade that
//! replaced the ring's output without consuming it. Past the loop's end the
//! count never came back under it, so every cycle flushed: a live looped voice
//! played 576 frames and then silence (doc 013, "The live disk loop").
//!
//! [`RegionOut::file_position`]: super::prefetch::RegionOut::file_position

use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::Arc;

use super::cache::LruCache;
use super::io::refill::load_wave;
use super::io::wave_io::{wave_frame_into, WaveIn};
use super::metrics::Metrics;
use super::plan::ChannelPlan;
use super::prefetch::RegionOut;
use tutti_core::{ChannelLayout, SampleRate};
use tutti_io::Wave;

use crate::nonempty;
use crate::voice::loop_span::{blend, LoopSpan};

/// A stream's loop as its refill writes it: the span, and the frames its fade
/// blends toward, captured once when the loop is set so no refill re-reads
/// them.
#[derive(Clone, Debug)]
pub(crate) struct RingLoop {
    span: LoopSpan,
    /// The `span.fade()` frames leading into `span.resume()` (the fade's
    /// lead-in, [`capture_lead_in`]), flat interleaved at the ring's width.
    /// Shared, so a parallel refill's work item holds it without a copy.
    lead_in: Arc<[f32]>,
}

impl RingLoop {
    /// The loop `[range.0, range.1)` with a `crossfade_frames` fade, on a file
    /// `len` frames long (its end clamped to the file, as every tier clamps
    /// it), writing a ring `channels` wide. `None` for a range with nothing in
    /// it, which does not loop.
    ///
    /// The lead-in comes from `wave`, the whole file; with none to read it
    /// from, the loop is hard (a fade of 0), so the span the ring is written by
    /// never claims a fade it cannot blend.
    pub(crate) fn capture(
        range: (u64, u64),
        crossfade_frames: usize,
        len: usize,
        wave: Option<&Wave>,
        channels: impl Into<ChannelLayout>,
    ) -> Option<Self> {
        let fade = if wave.is_some() { crossfade_frames } else { 0 };
        let span = LoopSpan::new(range.0 as usize, range.1 as usize, fade, len)?;
        let lead_in = match wave {
            Some(wave) if span.fade() > 0 => capture_lead_in(wave, &span, channels),
            _ => Vec::new(),
        };
        Some(Self {
            span,
            lead_in: lead_in.into(),
        })
    }

    /// The span the ring is written by.
    #[cfg(test)]
    pub(crate) fn span(&self) -> &LoopSpan {
        &self.span
    }

    /// Blend the frames of a run that fall in the fade toward their lead-in,
    /// as `interp::read_looped_frame` blends a tap: `run` holds file frames
    /// `from..`, `ch` wide.
    fn blend_run(&self, from: usize, run: &mut [f32], ch: usize) {
        let fade = self.span.fade();
        let fade_start = self.span.end() - fade;
        let frames = run.len() / ch;
        if fade == 0 || from + frames <= fade_start {
            return;
        }
        let first = fade_start.saturating_sub(from);
        for (k, frame) in run.chunks_exact_mut(ch).enumerate().skip(first) {
            let Some((lead, weight)) = self.span.fade_at(from + k) else {
                continue;
            };
            let at = (lead + fade - self.span.resume()) * ch;
            for (s, &l) in frame.iter_mut().zip(&self.lead_in[at..at + ch]) {
                *s = blend(*s, l, weight);
            }
        }
    }
}

/// Fill `out` (flat interleaved, `ch` wide) with the frames a forward stream
/// plays from straight position `pos` on: the file, or on `ring_loop` the
/// looped sequence, each run of file frames read by `read(frame, run)` and its
/// fade blended as it lands.
///
/// The one place the ring's sequence is made, for the refill (from the file
/// whole or from the disk decoder) and for the crossfade captures alike.
pub(crate) fn fill_sequence(
    pos: usize,
    out: &mut [f32],
    ch: usize,
    ring_loop: Option<&RingLoop>,
    mut read: impl FnMut(usize, &mut [f32]),
) {
    let frames = out.len() / ch;
    let mut done = 0;
    while done < frames {
        let (at, run) = match ring_loop {
            Some(ring_loop) => {
                let at = ring_loop.span.place_frame(pos + done);
                (at, (ring_loop.span.end() - at).min(frames - done))
            }
            None => (pos + done, frames - done),
        };
        let slice = &mut out[done * ch..(done + run) * ch];
        read(at, slice);
        if let Some(ring_loop) = ring_loop {
            ring_loop.blend_run(at, slice, ch);
        }
        done += run;
    }
}

/// Where the ring's head stands: the straight position of the next frame the
/// audio thread pops, the writer's cursor less what is buffered.
///
/// Forward streams only (a reverse writer's cursor moves the other way). A
/// snapshot: the audio thread may pop a few more frames before a flush this
/// informs lands, and those it hears again after it.
pub(crate) fn head_position(writer: &RegionOut) -> u64 {
    writer
        .file_position()
        .saturating_sub(writer.buffered().get() as u64)
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

/// Capture a loop's lead-in: the `fade` frames that lead into where the wrap
/// resumes, `[resume - fade, resume)` (`LoopSpan`'s rule), flat interleaved
/// at `channels`. `resume` is the loop's start, or — with too little before
/// the start for a lead-in — `start + fade`, so the lead-in is then the loop's
/// own head.
///
/// The frames that lead into `resume`, not the ones from it: the fade ends on
/// frame `resume - 1` and the wrap continues at `resume`, so the join is the
/// file's own step. Capturing `[start, start + fade)` and wrapping to `start`
/// — what this did first — faded into the loop's head and then played the
/// head again after the wrap: a jump of `fade` frames at every loop.
pub(crate) fn capture_lead_in(
    wave: &Wave,
    span: &LoopSpan,
    channels: impl Into<ChannelLayout>,
) -> Vec<f32> {
    capture_frames(wave, span.resume() - span.fade(), span.fade(), channels)
}

/// The loop a forward refill of `plan`'s stream writes by: `None` when it
/// does not loop, or plays in reverse (reverse ignores the loop on every
/// tier).
pub(crate) fn forward_loop(plan: &ChannelPlan) -> Option<&RingLoop> {
    if plan.rt_state.is_reverse() {
        return None;
    }
    plan.loop_config()?.ring.as_ref()
}

/// Capture the fadeout of a reposition's crossfade: the `count` frames the
/// ring hands the audio thread next, as the butler wrote them — the sequence
/// from the ring's head ([`head_position`]) under the loop it was written by.
///
/// Read from the wave file (via the cache) rather than by popping the SPSC
/// ring: the butler must never pop the consumer, which is the audio thread's
/// sole province (see [`ReaderCell`](crate::butler::prefetch::ReaderCell)'s
/// single-consumer invariant). This used to read the file at the reader's
/// `read_position` — a count of frames consumed, which is a file frame only
/// for a stream started at 0 that never looped or moved.
///
/// Empty for a reverse stream (its head is not a forward run), which skips
/// the crossfade: a hard cut, as an unloadable file gives.
pub(crate) fn ring_head_frames(
    plan: &ChannelPlan,
    writer: &RegionOut,
    cache: &LruCache,
    metrics: &Metrics,
    count: usize,
) -> Vec<f32> {
    if count == 0 || plan.link.is_none() || plan.rt_state.is_reverse() {
        return Vec::new();
    }
    sequence_frames(
        cache,
        metrics,
        writer.file_path(),
        head_position(writer),
        count,
        writer.channels(),
        forward_loop(plan),
    )
}

/// Capture `count` frames of the sequence a stream plays from straight
/// position `pos` (on `ring_loop`, when it loops), flat interleaved at
/// `channels` — the fadein of a reposition's crossfade, or its fadeout.
///
/// Empty when `count` is zero or the file cannot be loaded; both callers treat
/// an empty buffer as "skip the crossfade", which degrades to a hard cut rather
/// than to silence.
pub(crate) fn sequence_frames(
    cache: &LruCache,
    metrics: &Metrics,
    file_path: &Path,
    pos: u64,
    count: usize,
    channels: impl Into<ChannelLayout>,
    ring_loop: Option<&RingLoop>,
) -> Vec<f32> {
    if count == 0 {
        return Vec::new();
    }
    let Some(wave) = load_wave(cache, metrics, file_path) else {
        return Vec::new();
    };
    let channels = nonempty(channels.into());
    let ch = channels.count() as usize;
    let mut out = vec![0.0f32; count * ch];
    fill_sequence(pos as usize, &mut out, ch, ring_loop, |at, run| {
        WaveIn::new(&wave, at, channels).fill_interleaved(run);
    });
    out
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

    /// A stereo ring on a file that does not exist.
    fn test_ring() -> (RegionOut, crate::butler::prefetch::RegionReader) {
        crate::butler::RegionBuffer::with_capacity(
            crate::butler::RegionId(1),
            PathBuf::from("nonexistent.wav"),
            4096,
            2usize,
        )
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
    /// every tier reads by. A loop with no file to read its lead-in from is
    /// hard, not a fade toward silence.
    ///
    /// Mutation (run): `capture_lead_in` capturing from `start` in every mode
    /// (the old head replay) → `[5.0, 6.0]` → fails. Mutation (run): the head
    /// mode removed from `LoopSpan::new` (the fade clamped to `start`) → a
    /// 2-frame fade before frame 2 → fails. Mutation (run): `RingLoop::capture`
    /// keeping the fade without a wave → fails.
    #[test]
    fn a_loop_fades_in_from_what_leads_into_its_resume() {
        let wave = make_mono_wave(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0]);
        let ring = |range, fade| {
            RingLoop::capture(range, fade, wave.len(), Some(&wave), 1usize).expect("a loop")
        };
        let lead_in = |range, fade| ring(range, fade).lead_in.to_vec();
        assert_eq!(lead_in((5, 9), 2), [3.0, 4.0]);
        assert_eq!(ring((5, 7), 4).span().fade(), 2);
        assert_eq!(ring((2, 9), 4).span().fade(), 3);
        assert_eq!(lead_in((2, 9), 4), [2.0, 3.0, 4.0]);
        assert_eq!(ring((0, 9), 4).span().fade(), 4);
        assert_eq!(lead_in((0, 9), 4), [0.0, 1.0, 2.0, 3.0]);
        assert!(lead_in((0, 9), 0).is_empty());
        let blind = RingLoop::capture((5, 9), 2, 10, None, 1usize).expect("a loop");
        assert_eq!((blind.span().fade(), blind.lead_in.len()), (0, 0));
    }

    /// **The ring holds the loop as the sequence `LoopSpan` defines**: from
    /// frame 0, a crossfaded loop `[5, 9)` writes the file up to 5, blends
    /// 7 and 8 toward 3 and 4 as it writes them, and wraps to 5 — and a
    /// fill that starts past the loop's end (a straight position, as a seek
    /// or a refill hands it) places it on the loop first. Runs split at the
    /// loop's end, so each is a straight read of the file.
    ///
    /// Mutation (run): the blend dropped from `fill_sequence` → frame 7 reads
    /// 7.0 → fails. Mutation (run): `place_frame` wrapping to `start` in head
    /// mode → the second case reads 4.0 after 8 → fails. Mutation (run): the
    /// run not cut at the loop's end → 9.0 after 8 → fails.
    #[test]
    fn a_fill_writes_the_looped_sequence() {
        let wave = make_mono_wave(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0]);
        let read = |ring: &RingLoop, pos: usize, frames: usize| {
            let mut out = vec![0.0f32; frames];
            fill_sequence(pos, &mut out, 1, Some(ring), |at, run| {
                WaveIn::new(&wave, at, 1usize).fill_interleaved(run);
            });
            out
        };
        let lead = RingLoop::capture((5, 9), 2, wave.len(), Some(&wave), 1usize).expect("a loop");
        let blended = |tail: f32, lead: f32, k: f32| tail * (1.0 - k / 3.0) + lead * (k / 3.0);
        let (b7, b8) = (blended(7.0, 3.0, 1.0), blended(8.0, 4.0, 2.0));
        assert_eq!(
            read(&lead, 0, 13),
            [0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, b7, b8, 5.0, 6.0, b7, b8]
        );
        // Straight 11 is the loop's third frame, 7, on its second pass.
        assert_eq!(read(&lead, 11, 4), [b7, b8, 5.0, 6.0]);

        // Head mode: `[1, 9)` fading 3 into its own head resumes at 4.
        let head = RingLoop::capture((1, 9), 3, wave.len(), Some(&wave), 1usize).expect("a loop");
        let got = read(&head, 5, 8);
        let b = |tail: f32, lead: f32, k: f32| tail * (1.0 - k / 4.0) + lead * (k / 4.0);
        assert_eq!(
            got,
            [
                5.0,
                b(6.0, 1.0, 1.0),
                b(7.0, 2.0, 2.0),
                b(8.0, 3.0, 3.0),
                4.0,
                5.0,
                b(6.0, 1.0, 1.0),
                b(7.0, 2.0, 2.0)
            ]
        );
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
        let state = ChannelPlan::default();
        let (writer, _reader) = test_ring();
        let samples = ring_head_frames(&state, &writer, &cache, &metrics, 0);

        assert!(samples.is_empty());
    }

    #[test]
    fn test_capture_fadeout_no_consumer() {
        use crate::butler::plan::ChannelPlan;

        let cache = LruCache::new(10, 1024 * 1024);
        let metrics = Metrics::new();
        let state = ChannelPlan::default();
        let (writer, _reader) = test_ring();
        // No active link → nothing to fade out.
        let samples = ring_head_frames(&state, &writer, &cache, &metrics, 100);

        assert!(samples.is_empty());
    }

    #[test]
    fn test_capture_fadein_zero_count() {
        let cache = LruCache::new(10, 1024 * 1024);
        let metrics = Metrics::new();
        let path = PathBuf::from("nonexistent.wav");

        let samples = sequence_frames(&cache, &metrics, &path, 0, 0, 2usize, None);

        assert!(samples.is_empty());
    }

    #[test]
    fn test_capture_fadein_file_not_in_cache() {
        let cache = LruCache::new(10, 1024 * 1024);
        let metrics = Metrics::new();
        let path = PathBuf::from("nonexistent.wav");

        let samples = sequence_frames(&cache, &metrics, &path, 0, 100, 2usize, None);

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
