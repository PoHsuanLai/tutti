//! The read half of the streamer's cold-path disk refill.
//!
//! The streamer moves frames from a resident (whole-file) [`Wave`] into a
//! per-region ring buffer. This module owns the source end: [`read_wave`] and
//! [`wave_frame_into`] are the one place the planar `wave.at(0,i)/at(1,i)`
//! unpack lives, with mono up-mix and zero-pad past end. A loop is not its
//! business: the refill cuts its reads at the loop's end and places each run
//! (`loops::Mapping::fill`), so a read always runs the file straight on.
//!
//! Both ends speak flat interleaved `&[f32]` at a runtime width, with every
//! count denominated in frames. The sink end is
//! [`RegionOut::push_interleaved`](crate::streamer::prefetch::RegionOut::push_interleaved).

use crate::nonempty;
use crate::voice::interp::Place;
use tutti_codec::Wave;
use tutti_types::ChannelLayout;

/// The canonical planar→interleaved unpack: writes frame `idx` of `wave` into
/// `out` at `out.len()` channels, zero past the end.
///
/// # Channel policy
///
/// Identical to [`interp::read_frame`](crate::voice::interp::read_frame)'s,
/// through the same [`Place`]. The two tiers must agree here — this is the
/// streamer's unpack and that is the RT reader, and a voice that unpacked
/// differently on disk than in memory is precisely the divergence the shared
/// kernel exists to prevent.
#[inline]
pub(crate) fn wave_frame_into(wave: &Wave, place: &Place, idx: usize, out: &mut [f32]) {
    if out.is_empty() {
        return;
    }
    let src_ch = wave.channels();
    if idx >= wave.len() || src_ch == 0 {
        out.fill(0.0);
        return;
    }
    place.frame(src_ch, |c| wave.at(c, idx), out);
}

/// Read file frames `at..` of a resident `wave` into `out` (`channels` wide,
/// floored at one), zero past its end: the whole-file source a region without
/// a decoder reads, and the loop's reads of a resident file.
///
/// It fills the whole buffer, so it is not a [`Source`](tutti_types::Source):
/// it never ends and has no verdict to give. The caller owns the bound, by the
/// size of the scratch buffer it hands in.
pub(crate) fn read_wave(wave: &Wave, at: usize, out: &mut [f32], channels: ChannelLayout) {
    let channels = nonempty(channels);
    let place = Place::native(channels);
    let stride = channels.count() as usize;
    for (k, frame) in out.chunks_exact_mut(stride).enumerate() {
        wave_frame_into(wave, &place, at + k, frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_types::{SampleRate, Seconds};

    fn test_wave(samples: &[(f32, f32)]) -> Wave {
        let mut wave = Wave::new(2, SampleRate::new(48000.0));
        for &(l, r) in samples {
            wave.push_frame(&[l, r]);
        }
        wave
    }

    /// `wave_frame_into` with the placement a reader of `out`'s width has.
    fn frame_into(wave: &Wave, idx: usize, out: &mut [f32]) {
        let place = Place::for_wave(
            wave,
            ChannelLayout::try_from(out.len()).expect("a test width is small"),
        )
        .expect("placeable");
        wave_frame_into(wave, &place, idx, out);
    }

    fn indexed_wave(channels: usize, len: usize) -> Wave {
        let mut w = Wave::zero(
            channels,
            SampleRate::new(48000.0),
            Seconds::new(len as f64) / 48000.0,
        );
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
        let mut wave = Wave::new(1, SampleRate::new(48000.0));
        wave.push_frame(&[0.7]);
        let mut f = [0.0f32; 2];
        frame_into(&wave, 0, &mut f);
        assert_eq!(f, [0.7, 0.7]);
    }

    /// The streamer's unpack must follow the SAME channel policy as the RT
    /// reader's `interp::read_frame`: mono is placed by the spread, anything else by its topology. Two
    /// implementations of a channel policy is how the two tiers drift apart.
    #[test]
    fn wave_frame_matches_the_rt_readers_channel_policy() {
        // Mono goes to the centre of a 5.1 frame and nowhere else (the lone
        // centre rule of `Fold`; the memory tier's pinned in `interp`).
        let mut mono = Wave::new(1, SampleRate::new(48000.0));
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
        // The ITU figures, in the fold's order of operations.
        let m = tutti_types::M3DB;
        assert_eq!(
            narrow,
            [1.0 + m * 3.0 + m * 5.0, 2.0 + m * 3.0 + m * 6.0],
            "narrowing must fold, not truncate"
        );

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
    fn read_wave_polls_forward_frames() {
        let wave = test_wave(&[(0.1, 0.1), (0.2, 0.2), (0.3, 0.3)]);
        let mut out = [0.0f32; 6];
        read_wave(&wave, 0, &mut out, ChannelLayout::STEREO);
        assert_eq!(out, [0.1, 0.1, 0.2, 0.2, 0.3, 0.3]);
    }

    #[test]
    fn read_wave_zero_pads_past_end() {
        let wave = test_wave(&[(0.2, 0.2)]);
        let mut out = [9.0f32; 4];
        read_wave(&wave, 1, &mut out, ChannelLayout::STEREO);
        assert_eq!(out, [0.0; 4]);
    }

    #[test]
    fn read_wave_reads_six_channels() {
        let wave = indexed_wave(6, 4);
        let mut out = [0.0f32; 12];
        read_wave(&wave, 0, &mut out, ChannelLayout::from(6u16));
        for f in 0..2 {
            for c in 0..6 {
                assert_eq!(out[f * 6 + c], (c + 1) as f32, "frame {f} channel {c}");
            }
        }
    }
}
