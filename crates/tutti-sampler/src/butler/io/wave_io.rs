//! The read half of the butler's cold-path disk refill.
//!
//! The butler moves frames from a resident (whole-file) [`Wave`] into a
//! per-region ring buffer. This module owns the source end: [`WaveIn`] is the
//! one place the planar `wave.at(0,i)/at(1,i)` unpack lives — a cursor over a
//! `Wave` with mono up-mix and zero-pad past end. A loop is not its business:
//! the refill cuts its reads at the loop's end and places each run
//! (`loops::Mapping::fill`), so a `WaveIn` always reads the file straight on.
//!
//! Both ends speak flat interleaved `&[f32]` at a runtime width, with the
//! stride owned by the type and every count denominated in frames. The sink
//! end is [`RegionOut::push_interleaved`](crate::butler::prefetch::RegionOut::push_interleaved).
//! `WaveIn` deliberately does not implement [`AudioIn`](tutti_types::AudioIn) —
//! see its type docs, which is where that decision and its reasoning live.

use crate::voice::interp::MonoSpread;
use crate::{MAX_SAMPLER_CHANNELS, nonempty};
use tutti_codec::Wave;
use tutti_types::{ChannelLayout, fold_frame};

/// The canonical planar→interleaved unpack: writes frame `idx` of `wave` into
/// `out` at `out.len()` channels, zero past the end.
///
/// # Channel policy
///
/// Identical to [`interp::read_frame`](crate::voice::interp::read_frame)'s: a
/// mono source is placed by `mono` ([`MonoSpread`]); anything else folds through
/// [`fold_frame`]. The two tiers must agree here — this is the butler's
/// unpack and that is the RT reader, and a voice that unpacked differently on
/// disk than in memory is precisely the divergence the shared kernel exists to
/// prevent.
#[inline]
pub(crate) fn wave_frame_into(wave: &Wave, mono: &MonoSpread, idx: usize, out: &mut [f32]) {
    if out.is_empty() {
        return;
    }
    let src_ch = wave.channels();
    if idx >= wave.len() || src_ch == 0 {
        out.fill(0.0);
        return;
    }

    // A mono sample has no channel identity: the spread places it.
    if src_ch == 1 {
        mono.frame(wave.at(0, idx), out);
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

/// A forward reader over a resident `Wave`, reading from an internal cursor.
/// Past the end it yields silence, so it is an *unbounded* source — the
/// caller bounds the transfer by the size of the scratch buffer it fills.
///
/// # Deliberately NOT an [`AudioIn`](tutti_types::AudioIn), and not because of the width
///
/// Width is no obstacle: [`AudioIn`](tutti_types::AudioIn) carries a runtime
/// [`ChannelLayout`]. `WaveIn` stays inherent for a different reason, about
/// what a short count *means*.
///
/// [`AudioIn::poll_into`](tutti_types::AudioIn::poll_into)'s contract is entirely
/// about what a short or zero count means: `0` is either "the producer has
/// not caught up" ([`Starved`](tutti_types::OnEmpty::Starved)) or "there will
/// never be more" ([`EndOfStream`](tutti_types::OnEmpty::EndOfStream)), and
/// `ON_EMPTY` is the source answering that question once, on the type, so a
/// generic consumer can branch on it.
///
/// `WaveIn` cannot answer it, because it never asks it.
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
    /// Where a mono wave goes in a frame of [`channels`](Self::channels).
    mono: MonoSpread,
    cursor: usize,
}

impl<'w> WaveIn<'w> {
    /// A cursor over `wave` starting at frame `start`, emitting frames at
    /// `channels` wide. `channels` is floored at one — the output width is
    /// independent of `wave.channels()`, which [`wave_frame_into`]'s policy
    /// reconciles per frame.
    pub(crate) fn new(wave: &'w Wave, start: usize, channels: impl Into<ChannelLayout>) -> Self {
        let channels = nonempty(channels.into());
        Self {
            wave,
            channels,
            mono: MonoSpread::to(channels),
            cursor: start,
        }
    }

    /// Fill `out` with interleaved frames at this source's width, advancing the
    /// cursor. Always fills the whole buffer — past the end that means
    /// zero-padded silence.
    ///
    /// # Why the return is not an `AudioIn::poll_into` count
    ///
    /// It looks like one and it is not. The loop below is over
    /// `chunks_exact_mut`, with no early exit, so this returns
    /// `out.len() / ch` unconditionally — a pure function of the buffer the
    /// caller passed in, carrying no information back about the source. It is a
    /// convenience, not a signal, and the production callers
    /// (`loops::Mapping::fill`'s runs) discard it; the refill takes its frame
    /// count from `RegionOut::push_interleaved` instead. See the type-level docs for why
    /// that disqualifies `WaveIn` from the trait.
    pub(crate) fn fill_interleaved(&mut self, out: &mut [f32]) -> usize {
        // Stride derived once, above the frame loop.
        let ch = self.channels.count() as usize;
        let mut frames = 0;
        for frame in out.chunks_exact_mut(ch) {
            wave_frame_into(self.wave, &self.mono, self.cursor, frame);
            self.cursor += 1;
            frames += 1;
        }
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_wave(samples: &[(f32, f32)]) -> Wave {
        let mut wave = Wave::new(2, 48000.0);
        for &(l, r) in samples {
            wave.push_frame(&[l, r]);
        }
        wave
    }

    /// `wave_frame_into` with the spread a reader of `out`'s width has.
    fn frame_into(wave: &Wave, idx: usize, out: &mut [f32]) {
        let mono = MonoSpread::to(ChannelLayout::from(out.len()));
        wave_frame_into(wave, &mono, idx, out);
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
        frame_into(&wave, 0, &mut f);
        assert_eq!(f, [0.1, 0.2]);
        frame_into(&wave, 1, &mut f);
        assert_eq!(f, [0.3, 0.4]);
    }

    #[test]
    fn wave_frame_pads_zeros_past_end() {
        let wave = test_wave(&[(0.5, 0.6)]);
        let mut f = [9.0f32; 2];
        frame_into(&wave, 1, &mut f);
        assert_eq!(f, [0.0, 0.0]);
    }

    #[test]
    fn wave_frame_duplicates_left_for_mono() {
        let mut wave = Wave::new(1, 48000.0);
        wave.push_frame(&[0.7]);
        let mut f = [0.0f32; 2];
        frame_into(&wave, 0, &mut f);
        assert_eq!(f, [0.7, 0.7]);
    }

    /// The butler's unpack must follow the SAME channel policy as the RT
    /// reader's `interp::read_frame`: mono is placed by the spread, anything else folds. Two
    /// implementations of a channel policy is how the two tiers drift apart.
    #[test]
    fn wave_frame_matches_the_rt_readers_channel_policy() {
        // Mono goes to the centre of a 5.1 frame and nowhere else (the lone
        // centre rule of `Fold`; the memory tier's pinned in `interp`).
        let mut mono = Wave::new(1, 48000.0);
        mono.push_frame(&[0.4]);
        let mut f = [9.0f32; 6];
        frame_into(&mono, 0, &mut f);
        assert_eq!(f, [0.0, 0.0, 0.4, 0.0, 0.0, 0.0], "mono is the centre");

        // Six channels reach six slots.
        let six = indexed_wave(6, 4);
        let mut f = [0.0f32; 6];
        frame_into(&six, 0, &mut f);
        for (c, &s) in f.iter().enumerate() {
            assert_eq!(s, (c + 1) as f32, "channel {c} dropped: {f:?}");
        }

        // Narrowing folds through the engine matrix rather than truncating.
        let mut narrow = [0.0f32; 2];
        frame_into(&six, 0, &mut narrow);
        let mut expected = [0.0f32; 2];
        fold_frame(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], &mut expected);
        assert_eq!(narrow, expected, "narrowing must use fold_frame");

        // Stereo into a wide frame zero-fills rather than fanning.
        let stereo = indexed_wave(2, 4);
        let mut wide = [9.0f32; 6];
        frame_into(&stereo, 0, &mut wide);
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
        let mut src = WaveIn::new(&wave, 0, 2usize);
        let mut out = [0.0f32; 6];
        assert_eq!(src.fill_interleaved(&mut out), 3);
        assert_eq!(out, [0.1, 0.1, 0.2, 0.2, 0.3, 0.3]);
    }

    #[test]
    fn wave_in_zero_pads_past_end() {
        let wave = test_wave(&[(0.2, 0.2)]);
        let mut src = WaveIn::new(&wave, 1, 2usize);
        let mut out = [9.0f32; 4];
        src.fill_interleaved(&mut out);
        assert_eq!(out, [0.0; 4]);
    }

    #[test]
    fn wave_in_reads_six_channels() {
        let wave = indexed_wave(6, 4);
        let mut src = WaveIn::new(&wave, 0, 6usize);
        let mut out = [0.0f32; 12];
        assert_eq!(src.fill_interleaved(&mut out), 2);
        for f in 0..2 {
            for c in 0..6 {
                assert_eq!(out[f * 6 + c], (c + 1) as f32, "frame {f} channel {c}");
            }
        }
    }
}
