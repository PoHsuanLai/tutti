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
    use crate::{Dither, EncodeConfig, ExportConfig, Format, Pcm};

    fn config(pcm: Pcm, dither: Dither) -> ExportConfig {
        ExportConfig {
            encode: EncodeConfig {
                format: Format::Wav(pcm),
                ..Default::default()
            },
            dither,
            ..Default::default()
        }
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
                (Dither::Off, 1.0),
                (Dither::Rectangular, 1.5),
                (Dither::Triangular, 2.0),
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
        let int16 = config(Pcm::Int16, Dither::Triangular);
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

        let float = config(Pcm::Float32, Dither::Triangular);
        assert_eq!(
            Normalize::peak(Db(0.0)).within(&float),
            Normalize::peak(Db(0.0))
        );
    }
}
