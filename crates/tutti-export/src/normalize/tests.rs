use super::*;
use tutti_measure::Loudness;

fn reading(lufs: f32, true_peak: f32) -> Loudness {
    Loudness {
        lufs: Db(lufs),
        true_peak: Db(true_peak),
        range: Db(0.0),
    }
}

/// Peak mode targets TRUE peak, not sample peak — the whole reason it goes
/// through the R128 meter rather than a `fold(max, abs)` over the planes.
#[test]
fn peak_moves_the_true_peak_to_the_target() {
    let m = reading(-20.0, -6.0);
    let gain = Normalize::peak(Db(-1.0)).gain_for(&m);
    assert!(
        (gain.get() - 5.0).abs() < 1e-5,
        "−6 dBTP to a −1 dBTP target is +5 dB, got {gain:?}"
    );
}

/// Loudness mode defers to `gain_to`, ceiling included.
#[test]
fn lufs_respects_the_true_peak_ceiling() {
    // +6 dB would put a −2 dBTP peak at +4, which is 5 dB over a −1 ceiling.
    let m = reading(-20.0, -2.0);
    let gain = Normalize::lufs(Db(-14.0)).gain_for(&m);
    assert!(
        (gain.get() - 1.0).abs() < 1e-5,
        "the ceiling must pull the loudness gain back, got {gain:?}"
    );
}

/// `Normalize::lufs` carries the −1.0 dBTP ceiling, so callers do not have
/// to know the number.
#[test]
fn lufs_defaults_to_a_minus_one_dbtp_ceiling() {
    assert_eq!(
        Normalize::lufs(Db(-14.0)),
        Normalize::Lufs {
            target: Db(-14.0),
            ceiling: Db(-1.0)
        }
    );
}

#[cfg(feature = "wav")]
mod headroom {
    use super::*;
    use tutti_codec::{Format, Pcm};
    use tutti_types::ChannelLayout;

    fn config(pcm: Pcm, dither: Option<DitherShape>) -> FileTarget {
        FileTarget::new("unused.wav", Format::Wav(pcm), ChannelLayout::STEREO).with_dither(dither)
    }

    /// dBFS of `1 - lsbs / 2^(bits - 1)`: `lsbs` LSBs below full scale.
    fn below_full_scale(bits: u32, lsbs: f64) -> f32 {
        (20.0 * (1.0 - lsbs / f64::from(1u32 << (bits - 1))).log10()) as f32
    }

    /// An integer format keeps one LSB plus the dither's amplitude below full
    /// scale: none for no dither, half an LSB for rectangular, one for
    /// triangular.
    ///
    /// Mutation (run): `(1.0 + dither_peak)` → `1.0` in `headroom_ceiling` →
    /// the rectangular and triangular rows read 1 LSB's worth and fail.
    #[test]
    fn an_integer_format_keeps_an_lsb_and_its_dither_below_full_scale() {
        for (pcm, bits) in [(Pcm::Int16, 16), (Pcm::Int24, 24)] {
            for (dither, lsbs) in [
                (None, 1.0),
                (Some(DitherShape::Rectangular), 1.5),
                (Some(DitherShape::Triangular), 2.0),
            ] {
                let got = headroom_ceiling(&config(pcm, dither)).expect("quantizes");
                let want = below_full_scale(bits, lsbs);
                assert!(
                    (got.get() - want).abs() < 1e-7,
                    "{pcm:?} {dither:?}: {got:?} vs {want}"
                );
            }
        }
    }

    /// A target above the limit is pulled to it, one under it is left, the
    /// loudness target is never touched (only its ceiling), and a format that
    /// does not quantize has no limit.
    ///
    /// Mutation (run): `within` returning `self` → the 0 dBTP rows keep 0 →
    /// fails.
    #[test]
    fn a_target_above_the_limit_is_pulled_to_it() {
        let int16 = config(Pcm::Int16, Some(DitherShape::Triangular));
        let limit = Db(below_full_scale(16, 2.0));
        let near = |a: Db, b: Db| (a.get() - b.get()).abs() < 1e-7;

        let Normalize::Peak { target } = Normalize::peak(Db(0.0)).within(&int16) else {
            unreachable!()
        };
        assert!(near(target, limit), "{target:?}");
        assert_eq!(
            Normalize::peak(Db(-1.0)).within(&int16),
            Normalize::peak(Db(-1.0))
        );

        let Normalize::Lufs { target, ceiling } = Normalize::Lufs {
            target: Db(-14.0),
            ceiling: Db(0.5),
        }
        .within(&int16) else {
            unreachable!()
        };
        assert_eq!(target, Db(-14.0));
        assert!(near(ceiling, limit), "{ceiling:?}");
        assert_eq!(
            Normalize::lufs(Db(-14.0)).within(&int16),
            Normalize::lufs(Db(-14.0)),
            "the default -1 dBTP ceiling is under the limit"
        );

        let float = config(Pcm::Float32, Some(DitherShape::Triangular));
        assert_eq!(
            Normalize::peak(Db(0.0)).within(&float),
            Normalize::peak(Db(0.0))
        );
    }
}

mod measurer {
    use super::*;
    use crate::control::{CancelToken, Progress};
    use crate::{Outcome, Rendered};
    use std::f32::consts::TAU;
    use tutti_measure::{LoudnessConfig, measure_loudness};

    const RATE: SampleRate = SampleRate(48_000.0);

    /// A 1 kHz sine of `secs` seconds, as planes: amplitude `amp` on the left
    /// and half of it on the right, so a measurement that put a sample on the
    /// wrong channel reads something else.
    fn sine(secs: usize, amp: f32) -> Rendered {
        let plane = |amp: f32| -> Vec<f32> {
            (0..secs * 48_000)
                .map(|i| amp * (TAU * 1_000.0 * i as f32 / 48_000.0).sin())
                .collect()
        };
        Rendered::new(vec![plane(amp), plane(amp / 2.0)], RATE, 0).expect("equal planes")
    }

    /// `measure` of `rendered` under a control that reports to `report`,
    /// which may cancel `token`.
    fn measure(
        rendered: &Rendered,
        token: &CancelToken,
        report: &mut dyn FnMut(&Progress),
    ) -> Outcome<Loudness> {
        let mut control = Control::new(token).on_progress(report);
        control.weigh(Weights::Normalized);
        Normalize::peak(Db(-1.0))
            .measure(rendered, Samples(0), rendered.frames(), &mut control)
            .expect("a measurable sine")
    }

    /// The chunk is one second of audio, at least a frame.
    ///
    /// Mutation (run): `chunk_frames` returning twice the rate -> fails here
    /// and in the cancel test below.
    #[test]
    fn a_chunk_is_at_most_a_second() {
        assert_eq!(chunk_frames(RATE), 48_000);
        assert_eq!(chunk_frames(SampleRate(44_100.0)), 44_100);
        assert_eq!(chunk_frames(SampleRate(0.2)), 1);
    }

    /// The chunked reading of a known sine is its analytic loudness, and the
    /// one-shot reading of the same buffer (`measure_loudness`, which this
    /// crate used before) is the same figure to within the measurement's own
    /// stated tolerance (0.01 LU, `tutti-measure`'s streaming test).
    ///
    /// Derived from BS.1770, not from the implementation: a sine of amplitude
    /// a on the left and a/2 on the right reads `-0.691 + 10 log10(G * 0.625 *
    /// a^2)` LUFS, and K-weighting's gain G at 997 Hz is +0.691 dB, which
    /// is what the constant cancels, so it reads `20 log10 a` plus
    /// `10 log10 0.625` (the mean squares, a^2/2 and a^2/8, sum to 0.625 a^2): -22.04 LUFS at a = 0.1. Its true peak is the larger
    /// amplitude, -20 dBTP. The tolerance on both (0.1) covers the gain at
    /// 1 kHz not being the one at 997 Hz.
    ///
    /// Mutation (run): `Measurer::feed` interleaving `&planes[..1]` (the right
    /// channel reads silence) -> the loudness is off by 1 LU and the one-shot
    /// comparison by as much; fails.
    #[test]
    fn the_chunked_reading_is_the_analytic_one_and_the_one_shot_one() {
        let rendered = sine(5, 0.1);
        let chunked = measure(&rendered, &CancelToken::new(), &mut |_| {})
            .finished()
            .expect("not cancelled");

        let analytic_lufs = 20.0 * 0.1f32.log10() + 10.0 * 0.625f32.log10();
        assert!(
            (chunked.lufs.get() - analytic_lufs).abs() < 0.1,
            "{:?} vs {analytic_lufs}",
            chunked.lufs
        );
        assert!(
            (chunked.true_peak.get() - -20.0).abs() < 0.1,
            "{:?}",
            chunked.true_peak
        );

        let one_shot = measure_loudness(
            &LoudnessConfig::new(RATE, rendered.layout()),
            Interleaved::new(&rendered.interleaved(), rendered.layout()),
        )
        .expect("meterable");
        assert!(
            (chunked.lufs.get() - one_shot.lufs.get()).abs() < 0.01,
            "{:?} vs {:?}",
            chunked.lufs,
            one_shot.lufs
        );
        assert!((chunked.true_peak.get() - one_shot.true_peak.get()).abs() < 0.01);
        assert!((chunked.range.get() - one_shot.range.get()).abs() < 0.01);
    }

    /// Where the stream is cut does not move the reading: 777-frame spans
    /// (not a divisor of anything here) read what one-second ones do.
    ///
    /// Mutation (run): `Measurer::feed` sizing its scratch one sample long
    /// (a ragged tail the meter carries onto the next span's first channel)
    /// -> the readings differ; fails.
    #[test]
    fn the_cut_does_not_move_the_reading() {
        let rendered = sine(3, 0.25);
        let whole = measure(&rendered, &CancelToken::new(), &mut |_| {})
            .finished()
            .expect("not cancelled");
        let mut fine = Measurer::new(RATE, rendered.layout()).expect("meterable");
        let frames = rendered.frames().get();
        let mut at = 0;
        while at < frames {
            let end = (at + 777).min(frames);
            fine.feed(rendered.planes(), at..end);
            at = end;
        }
        let fine = fine.finish();
        assert!((fine.lufs.get() - whole.lufs.get()).abs() < 0.01);
        assert!((fine.true_peak.get() - whole.true_peak.get()).abs() < 0.01);
    }

    /// A cancel between chunks stops the measurement there: the callback
    /// cancels on the second chunk's reading, and no third chunk is fed.
    ///
    /// Mutation (run): the `is_cancelled` check at the top of `measure`'s
    /// chunk loop removed -> the other check (after the loop) still ends it,
    /// but after five chunks; fails.
    #[test]
    fn a_cancel_between_chunks_stops_the_measurement() {
        let rendered = sine(5, 0.1);
        let token = CancelToken::new();
        let mut reported = 0;
        let outcome = measure(&rendered, &token, &mut |p| {
            reported += 1;
            if p.frames().is_some_and(|(done, _)| done.get() >= 2 * 48_000) {
                token.cancel();
            }
        });
        assert_eq!(outcome, Outcome::Cancelled);
        // The entry is `begin`'s, not `measure`'s; chunks 1 and 2 are the only
        // readings that are, and the cadence delivers every 4096 frames, so a
        // chunk is one reading.
        assert_eq!(reported, 2, "two chunks were measured, then it stopped");
    }
}
