use super::*;

const RATE: SampleRate = SampleRate(48_000.0);

/// A 1 s loop (48 000 frames) at RT60 1 s loses 60 dB per trip: the gain is
/// 10^-3 exactly. At RT60 2 s it loses 30 dB: 10^-1.5.
#[test]
fn a_loop_that_decays_sixty_decibels_per_trip_has_a_thousandth_gain() {
    assert!((loop_gain(48_000.0, Seconds(1.0), RATE).get() - 1e-3).abs() < 1e-9);
    let g = loop_gain(48_000.0, Seconds(2.0), RATE);
    assert!((g.get() - 10f32.powf(-1.5)).abs() < 1e-6, "{g}");
}

/// The two functions are inverses, over delays, decays and rates.
#[test]
fn rt60_from_gain_inverts_loop_gain() {
    for rate in [44_100.0, 48_000.0, 96_000.0] {
        for delay in [1.0f32, 37.5, 1116.0, 40_000.0] {
            for rt60 in [0.05f32, 0.5, 2.0, 8.0] {
                let g = loop_gain(delay, Seconds::from_f32(rt60), SampleRate(rate));
                if g.get() <= 1e-6 {
                    continue; // below f32's resolution of a gain
                }
                let back = rt60_from_gain(g, delay, SampleRate(rate)).to_f32();
                assert!((back - rt60).abs() <= 2e-3 * rt60, "{back} vs {rt60}");
            }
        }
    }
}

/// The RT60 is the formula in `f64`, returned exact: delay 100 at gain 0.5
/// and 48 kHz is `-3 * 100 / (48000 * log10 0.5)` = 0.020763936...  s, to the
/// bit. An `f32` rounding of the result differs.
#[test]
fn rt60_from_gain_is_exact() {
    let want = -3.0 * 100.0 / (48_000.0 * libm::log10(0.5));
    assert_eq!(
        rt60_from_gain(Amplitude(0.5), 100.0, SampleRate(48_000.0)),
        Seconds(want)
    );
}

/// The edges: no memory is gain 0, never decaying is gain 1 and the inverse
/// reads those back, a gain above 1 or NaN is unbounded, a zero delay at
/// gain below 1 decays at once.
#[test]
fn the_edges_of_the_formula() {
    assert_eq!(loop_gain(100.0, Seconds(0.0), RATE), Amplitude::SILENT);
    assert_eq!(loop_gain(100.0, Seconds(f64::NAN), RATE), Amplitude::SILENT);
    assert_eq!(
        loop_gain(100.0, Seconds(f64::INFINITY), RATE),
        Amplitude::UNITY
    );
    assert_eq!(rt60_from_gain(Amplitude(0.0), 100.0, RATE).to_f32(), 0.0);
    assert_eq!(
        rt60_from_gain(Amplitude(1.0), 100.0, RATE).to_f32(),
        f32::INFINITY
    );
    assert_eq!(
        rt60_from_gain(Amplitude(1.2), 100.0, RATE).to_f32(),
        f32::INFINITY
    );
    assert_eq!(
        rt60_from_gain(Amplitude(f32::NAN), 100.0, RATE).to_f32(),
        f32::INFINITY
    );
    assert_eq!(
        rt60_from_gain(Amplitude(-0.5), 100.0, RATE).to_f32(),
        f32::INFINITY
    );
    assert_eq!(rt60_from_gain(Amplitude(0.5), 0.0, RATE).to_f32(), 0.0);
}
