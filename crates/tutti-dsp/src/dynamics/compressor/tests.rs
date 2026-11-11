use super::*;

const RATE: f64 = 48_000.0;

fn block(threshold: f32, ratio: f32) -> CompressorBlock {
    CompressorBlock {
        threshold: Db(threshold),
        knee: Db(0.0),
        ratio: CompressionRatio(ratio),
        makeup_from: Db(0.0),
        makeup_to: Db(0.0),
    }
}

/// A steady sidechain `over` dB above a hard-knee threshold settles to a
/// reduction of `over * (1 - 1/ratio)` dB, and the gain applied is that
/// reduction as an amplitude.
#[test]
fn a_steady_level_settles_to_the_static_curve() {
    for (threshold, level_db, ratio) in [(-20.0f32, -10.0f32, 4.0f32), (-30.0, -6.0, 2.0)] {
        let mut k = CompressorKernel::new(Seconds(0.001), Seconds(0.05), SampleRate::new(RATE));
        let b = block(threshold, ratio);
        let level = db_to_amplitude(Db(level_db)).get();
        let mut gain = 1.0;
        for i in 0..20_000 {
            gain = k.gain(&b, i % 64, 64, level, None);
        }
        let want = (level_db - threshold) * (1.0 - 1.0 / ratio);
        assert!(
            (k.gain_reduction_db().get() - want).abs() < 1e-2,
            "{} vs {want}",
            k.gain_reduction_db().get()
        );
        let want_gain = db_to_amplitude(Db(-want)).get();
        assert!((gain - want_gain).abs() < 1e-3, "{gain} vs {want_gain}");
        assert_eq!(k.envelope_level().get(), level);
    }
}

/// A level under the threshold is not reduced.
#[test]
fn a_level_under_the_threshold_is_left_alone() {
    let mut k = CompressorKernel::new(Seconds(0.0), Seconds(0.0), SampleRate::new(RATE));
    let level = db_to_amplitude(Db(-40.0)).get();
    let g = k.gain(&block(-20.0, 4.0), 0, 1, level, None);
    assert_eq!(g, 1.0);
}

/// A non-finite per-sample threshold override falls back to the block's
/// threshold, bit for bit; a finite one is honoured.
#[test]
fn a_non_finite_threshold_override_falls_back_to_the_block() {
    let b = block(-20.0, 4.0);
    let level = db_to_amplitude(Db(-10.0)).get();
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut held = CompressorKernel::new(Seconds(0.0), Seconds(0.0), SampleRate::new(RATE));
        let mut fed = held.clone();
        let want = held.gain(&b, 0, 1, level, None);
        let got = fed.gain(&b, 0, 1, level, Some(Db(bad)));
        assert_eq!(got.to_bits(), want.to_bits(), "{bad}");
    }
    let mut held = CompressorKernel::new(Seconds(0.0), Seconds(0.0), SampleRate::new(RATE));
    let mut lowered = held.clone();
    let a = held.gain(&b, 0, 1, level, None);
    let c = lowered.gain(&b, 0, 1, level, Some(Db(-30.0)));
    assert!(c < a, "a lower threshold reduces more: {c} vs {a}");
}
