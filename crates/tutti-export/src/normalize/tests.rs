use super::*;
use tutti_dsp::Loudness;

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
