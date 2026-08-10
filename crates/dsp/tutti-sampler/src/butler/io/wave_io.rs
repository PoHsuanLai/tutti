//! The read half of the butler's cold-path disk refill.
//!
//! The butler moves frames from a resident (whole-file) [`Wave`] into a
//! per-region ring buffer. This module owns the source end: [`WaveIn`] is the
//! one place the planar `wave.at(0,i)/at(1,i)` unpack lives — a cursor over a
//! `Wave` with mono up-mix, optional loop wrap, and zero-pad past end.
//!
//! Both ends speak flat interleaved `&[f32]` at a **runtime** width, with the
//! stride owned by the type and every count denominated in **frames**. The sink
//! end, [`RegionOut`](crate::butler::prefetch::RegionOut), states that shape as
//! the engine's [`AudioOut`](tutti_core::AudioOut) trait. `WaveIn` deliberately
//! does not implement the matching [`AudioIn`](tutti_core::AudioIn) — see its
//! type docs, which is where that decision and its reasoning live.

use crate::{nonempty, MAX_SAMPLER_CHANNELS};
use tutti_core::{fold_frame, ChannelLayout, Wave};

/// The canonical planar→interleaved unpack: writes frame `idx` of `wave` into
/// `out` at `out.len()` channels, zero past the end.
///
/// # Channel policy
///
/// Identical to [`interp::read_frame`](crate::voice::interp::read_frame)'s: a
/// mono source fans to every channel; anything else folds through
/// [`fold_frame`]. The two tiers **must** agree here — this is the butler's
/// unpack and that is the RT reader, and a voice that unpacked differently on
/// disk than in memory is precisely the divergence the shared kernel exists to
/// prevent.
#[inline]
pub(crate) fn wave_frame_into(wave: &Wave, idx: usize, out: &mut [f32]) {
    if out.is_empty() {
        return;
    }
    let src_ch = wave.channels();
    if idx >= wave.len() || src_ch == 0 {
        out.fill(0.0);
        return;
    }

    // Mono fans out — a mono sample has no channel identity.
    if src_ch == 1 {
        out.fill(wave.at(0, idx));
        return;
    }

    // Matched width: straight per-channel read. Bound: `src_ch == out.len()`,
    // which is what keeps `Wave::at` (an unchecked index) from panicking.
    if src_ch == out.len() {
        for (c, o) in out.iter_mut().enumerate() {
            *o = wave.at(c, idx);
        }
        return;
    }

    // Mismatched: gather at the source width into a bounded stack frame and let
    // the engine's one matrix decide the mapping.
    let n = src_ch.min(MAX_SAMPLER_CHANNELS);
    let mut src = [0.0f32; MAX_SAMPLER_CHANNELS];
    for (c, s) in src.iter_mut().enumerate().take(n) {
        *s = wave.at(c, idx);
    }
    fold_frame(&src[..n], out);
}

/// Wrap `pos` back into `[start, end)` when it has run past the end. All three
/// are file **frames**.
///
/// Modulo, not a single subtraction: at high varispeed one advance can overshoot
/// a short loop by more than its own length, and subtracting once would land
/// outside the region.
///
/// # Panics
///
/// Callers guarantee `end > start`; an empty range divides by zero.
#[inline]
pub(crate) fn wrap_into(pos: usize, start: usize, end: usize) -> usize {
    if pos >= end {
        start + ((pos - start) % (end - start))
    } else {
        pos
    }
}

/// Wrap `pos` into the half-open loop range if it has run past the end.
///
/// `pos` and `loop_range` are file **frames**; `None`, or a range that is empty
/// or inverted, is the identity — which is what makes this safe to call where
/// [`wrap_into`] would divide by zero. The single source of truth for the
/// loop-wrap arithmetic shared by [`WaveIn`] and the streaming/whole-file
/// forward refills.
#[inline]
pub(crate) fn wrap_position(pos: usize, loop_range: Option<(u64, u64)>) -> usize {
    match loop_range {
        Some((start, end)) if end > start => wrap_into(pos, start as usize, end as usize),
        _ => pos,
    }
}

/// A forward reader over a resident `Wave`, reading from an internal cursor
/// with optional loop wrap. Past the end (with no loop) it yields silence, so it
/// is an *unbounded* source — the caller bounds the transfer by the size of the
/// scratch buffer it fills.
///
/// # Deliberately NOT an [`AudioIn`], and not because of the width
///
/// Width is no obstacle: [`AudioIn`](tutti_core::AudioIn) and
/// [`AudioOut`](tutti_core::AudioOut) carry a runtime [`ChannelLayout`], which is
/// exactly why [`RegionOut`](crate::butler::prefetch::RegionOut) — the sink at
/// the other end of this refill — does implement `AudioOut`. `WaveIn` stays
/// inherent for a different reason, about what a short count *means*.
///
/// [`AudioIn::poll_into`](tutti_core::AudioIn::poll_into)'s contract is entirely
/// about what a **short or zero count means**: `0` is either "the producer has
/// not caught up" ([`Starved`](tutti_core::OnEmpty::Starved)) or "there will
/// never be more" ([`EndOfStream`](tutti_core::OnEmpty::EndOfStream)), and
/// `ON_EMPTY` is the source answering that question once, on the type, so a
/// generic consumer can branch on it.
///
/// **`WaveIn` cannot answer it, because it never asks it.**
/// [`fill_interleaved`](Self::fill_interleaved) always fills the whole buffer
/// and always returns `out.len() / channels` — it is unbounded by construction,
/// yielding silence forever past the end of the wave. Neither verdict is true of
/// it:
///
/// - `EndOfStream` is a promise about the *first* zero. `WaveIn` runs off the
///   end of the wave and keeps returning full counts of silence, so a consumer
///   looping on that verdict would never stop.
/// - `Starved` promises a producer that will catch up. There is no producer —
///   the `Wave` is resident in memory, and a retry returns exactly the same
///   silence.
///
/// A third `OnEmpty` variant ("unbounded — pads rather than ending") would make
/// it fit, but that is the wrong trade: it would add a case every existing
/// consumer must handle, in `tutti-types`, to describe a source whose count is
/// already known to be constant. The honest reading is that a source which never
/// returns a short count is not answering the question `AudioIn` exists to ask,
/// so it is not an `AudioIn`. It stays inherent, and the *caller* owns the
/// bound — which is what `refill_forward` already does by sizing the scratch
/// buffer.
pub(crate) struct WaveIn<'w> {
    wave: &'w Wave,
    /// Output interleave width — independent of `wave.channels()`, which
    /// [`wave_frame_into`]'s policy reconciles per frame.
    channels: ChannelLayout,
    cursor: usize,
    /// `(start, end)` half-open loop bounds in **frames**, if looping.
    /// Pre-validated non-empty at construction.
    loop_bounds: Option<(usize, usize)>,
}

impl<'w> WaveIn<'w> {
    /// A cursor over `wave` starting at frame `start`, emitting frames at
    /// `channels` wide.
    ///
    /// `loop_range` is `(start, end)` in file **frames**, half-open; an empty or
    /// inverted range is treated as no loop, so the reader runs off the end into
    /// silence instead of wrapping on a degenerate region. `channels` is floored
    /// at one — the output width is independent of `wave.channels()`, which
    /// [`wave_frame_into`]'s policy reconciles per frame.
    pub(crate) fn new(
        wave: &'w Wave,
        start: usize,
        loop_range: Option<(u64, u64)>,
        channels: impl Into<ChannelLayout>,
    ) -> Self {
        let loop_bounds = loop_range.and_then(|(start, end)| {
            let start = start as usize;
            let end = end as usize;
            (end > start).then_some((start, end))
        });
        Self {
            wave,
            channels: nonempty(channels.into()),
            cursor: start,
            loop_bounds,
        }
    }

    /// Fill `out` with interleaved frames at this source's width, advancing the
    /// cursor (with loop wrap). Always fills the whole buffer — past the end
    /// with no loop that means zero-padded silence.
    ///
    /// # Why the return is not an `AudioIn::poll_into` count
    ///
    /// It looks like one and it is not. The loop below is over
    /// `chunks_exact_mut`, with no early exit, so this returns
    /// `out.len() / ch` **unconditionally** — a pure function of the buffer the
    /// caller passed in, carrying no information back about the source. It is a
    /// convenience, not a signal, and the one production caller
    /// (`refill::refill_forward`) discards it and takes its frame count from
    /// `RegionOut::push_interleaved` instead. See the type-level docs for why
    /// that disqualifies `WaveIn` from the trait.
    pub(crate) fn fill_interleaved(&mut self, out: &mut [f32]) -> usize {
        // Stride derived once, above the frame loop.
        let ch = self.channels.count() as usize;
        let mut frames = 0;
        for frame in out.chunks_exact_mut(ch) {
            self.cursor = self.wrap(self.cursor);
            wave_frame_into(self.wave, self.cursor, frame);
            self.cursor += 1;
            frames += 1;
        }
        self.cursor = self.wrap(self.cursor);
        frames
    }

    /// `loop_bounds` is pre-validated non-empty at construction, so this only
    /// has to apply the shared arithmetic.
    #[inline]
    fn wrap(&self, pos: usize) -> usize {
        match self.loop_bounds {
            Some((start, end)) => wrap_into(pos, start, end),
            None => pos,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_wave(samples: &[(f32, f32)]) -> Wave {
        let mut wave = Wave::new(2, 48000.0);
        for &(l, r) in samples {
            wave.push((l, r));
        }
        wave
    }

    fn indexed_wave(channels: usize, len: usize) -> Wave {
        let mut w = Wave::zero(channels, 48000.0, len as f64 / 48000.0);
        for i in 0..w.len() {
            for c in 0..channels {
                w.set(c, i, (c + 1) as f32);
            }
        }
        w
    }

    #[test]
    fn wave_frame_reads_stereo() {
        let wave = test_wave(&[(0.1, 0.2), (0.3, 0.4)]);
        let mut f = [0.0f32; 2];
        wave_frame_into(&wave, 0, &mut f);
        assert_eq!(f, [0.1, 0.2]);
        wave_frame_into(&wave, 1, &mut f);
        assert_eq!(f, [0.3, 0.4]);
    }

    #[test]
    fn wave_frame_pads_zeros_past_end() {
        let wave = test_wave(&[(0.5, 0.6)]);
        let mut f = [9.0f32; 2];
        wave_frame_into(&wave, 1, &mut f);
        assert_eq!(f, [0.0, 0.0]);
    }

    #[test]
    fn wave_frame_duplicates_left_for_mono() {
        let mut wave = Wave::new(1, 48000.0);
        wave.push(0.7);
        let mut f = [0.0f32; 2];
        wave_frame_into(&wave, 0, &mut f);
        assert_eq!(f, [0.7, 0.7]);
    }

    /// The butler's unpack must follow the SAME channel policy as the RT
    /// reader's `interp::read_frame`: mono fans, anything else folds. Two
    /// implementations of a channel policy is how the two tiers drift apart.
    #[test]
    fn wave_frame_matches_the_rt_readers_channel_policy() {
        // Mono fans to every channel.
        let mut mono = Wave::new(1, 48000.0);
        mono.push(0.4);
        let mut f = [0.0f32; 6];
        wave_frame_into(&mono, 0, &mut f);
        assert!(
            f.iter().all(|&s| (s - 0.4).abs() < 1e-6),
            "mono must fan: {f:?}"
        );

        // Six channels reach six slots.
        let six = indexed_wave(6, 4);
        let mut f = [0.0f32; 6];
        wave_frame_into(&six, 0, &mut f);
        for (c, &s) in f.iter().enumerate() {
            assert_eq!(s, (c + 1) as f32, "channel {c} dropped: {f:?}");
        }

        // Narrowing folds through the engine matrix rather than truncating.
        let mut narrow = [0.0f32; 2];
        wave_frame_into(&six, 0, &mut narrow);
        let mut expected = [0.0f32; 2];
        fold_frame(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], &mut expected);
        assert_eq!(narrow, expected, "narrowing must use fold_frame");

        // Stereo into a wide frame zero-fills rather than fanning.
        let stereo = indexed_wave(2, 4);
        let mut wide = [9.0f32; 6];
        wave_frame_into(&stereo, 0, &mut wide);
        assert_eq!(wide[0], 1.0);
        assert_eq!(wide[1], 2.0);
        assert!(
            wide[2..].iter().all(|&s| s == 0.0),
            "surrounds must be silent: {wide:?}"
        );
    }

    #[test]
    fn wave_in_polls_forward_frames() {
        let wave = test_wave(&[(0.1, 0.1), (0.2, 0.2), (0.3, 0.3)]);
        let mut src = WaveIn::new(&wave, 0, None, 2usize);
        let mut out = [0.0f32; 6];
        assert_eq!(src.fill_interleaved(&mut out), 3);
        assert_eq!(out, [0.1, 0.1, 0.2, 0.2, 0.3, 0.3]);
    }

    #[test]
    fn wave_in_zero_pads_past_end() {
        let wave = test_wave(&[(0.2, 0.2)]);
        let mut src = WaveIn::new(&wave, 1, None, 2usize);
        let mut out = [9.0f32; 4];
        src.fill_interleaved(&mut out);
        assert_eq!(out, [0.0; 4]);
    }

    #[test]
    fn wave_in_wraps_within_loop() {
        // samples 0..4, loop [1,3): after index 2 the next read wraps to 1.
        let wave = test_wave(&[(0.0, 0.0), (1.0, 1.0), (2.0, 2.0), (3.0, 3.0)]);
        let mut src = WaveIn::new(&wave, 1, Some((1, 3)), 2usize);
        let mut out = [0.0f32; 10];
        src.fill_interleaved(&mut out);
        // 1,2 then wrap -> 1,2 then 1
        assert_eq!(out, [1.0, 1.0, 2.0, 2.0, 1.0, 1.0, 2.0, 2.0, 1.0, 1.0]);
    }

    #[test]
    fn wave_in_reads_six_channels() {
        let wave = indexed_wave(6, 4);
        let mut src = WaveIn::new(&wave, 0, None, 6usize);
        let mut out = [0.0f32; 12];
        assert_eq!(src.fill_interleaved(&mut out), 2);
        for f in 0..2 {
            for c in 0..6 {
                assert_eq!(out[f * 6 + c], (c + 1) as f32, "frame {f} channel {c}");
            }
        }
    }

    #[test]
    fn wrap_position_identity_without_loop() {
        assert_eq!(wrap_position(250, None), 250);
    }

    #[test]
    fn wrap_position_wraps_past_loop_end() {
        assert_eq!(wrap_position(200, Some((100, 200))), 100);
        assert_eq!(wrap_position(250, Some((100, 200))), 150);
        assert_eq!(wrap_position(150, Some((100, 200))), 150); // before end: identity
    }
}
