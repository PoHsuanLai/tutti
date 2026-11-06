use super::*;
use crate::config::EncodeConfig;

fn config(dither: Dither, bit_depth: BitDepth) -> ExportConfig {
    ExportConfig {
        encode: EncodeConfig {
            bit_depth,
            ..Default::default()
        },
        dither,
        ..Default::default()
    }
}

/// At `Float32` nothing may be added at all — the guard whose absence makes
/// `1 << 31` overflow to a negative LSB.
#[test]
fn float32_is_never_dithered() {
    let mut d = DitherState::for_config(&config(Dither::Triangular, BitDepth::Float32));
    assert!(d.lsb.is_none());
    let mut samples = [0.5f32; 16];
    d.apply(&mut samples);
    assert!(
        samples.iter().all(|&s| s == 0.5),
        "float output must pass through untouched, got {:?}",
        samples[0]
    );
}

/// An Ogg export at the default integer depth and dither adds nothing.
///
/// Mutation (run): drop the `OggVorbis` arm in `for_config` and the noise
/// reaches the samples.
#[test]
fn ogg_is_never_dithered() {
    let mut cfg = config(Dither::Triangular, BitDepth::Int24);
    cfg.encode.format = AudioFormat::OggVorbis(Default::default());
    let mut d = DitherState::for_config(&cfg);
    assert!(d.is_off());
    let mut samples = [0.5f32; 16];
    d.apply(&mut samples);
    assert!(samples.iter().all(|&s| s == 0.5));
}

#[test]
fn off_is_a_no_op() {
    let mut d = DitherState::for_config(&config(Dither::Off, BitDepth::Int16));
    let mut samples = [0.25f32, -0.25, 0.25, -0.25, 0.25, -0.25, 0.25, -0.25];
    d.apply(&mut samples);
    assert_eq!(
        samples,
        [0.25, -0.25, 0.25, -0.25, 0.25, -0.25, 0.25, -0.25]
    );
}

/// The noise is bounded by one LSB at the target depth — audible dither
/// would be a bug, and a wrong-sign or wrong-scale LSB shows up here.
#[test]
fn noise_stays_within_one_lsb() {
    for depth in [BitDepth::Int16, BitDepth::Int24] {
        let mut d = DitherState::for_config(&config(Dither::Triangular, depth));
        let lsb = d.lsb.expect("integer depths dither");
        assert!(lsb > 0.0, "LSB must be positive, got {lsb}");
        let mut samples = [0.0f32; 512];
        d.apply(&mut samples);
        for &s in &samples {
            assert!(s.abs() <= lsb, "|{s}| exceeded one LSB ({lsb})");
        }
    }
}

/// The RNG advances once per SAMPLE, and the sequence continues across
/// calls. Dithering one block of N samples must give exactly what two
/// blocks of N/2 give — the property that keeps the noise from correlating
/// to the block grid, and the one a width-aware rewrite could break.
#[test]
fn the_noise_sequence_is_continuous_across_blocks() {
    let cfg = config(Dither::Triangular, BitDepth::Int16);

    let mut whole = [0.0f32; 16];
    DitherState::for_config(&cfg).apply(&mut whole);

    let mut split = [0.0f32; 16];
    let mut d = DitherState::for_config(&cfg);
    let (a, b) = split.split_at_mut(6);
    d.apply(a);
    d.apply(b);

    assert_eq!(
        whole, split,
        "a split block must draw the same noise as one whole block"
    );
}

/// A 16-bit LSB is 2^-15; 24-bit is 2^-23. Derived from the depth, not read
/// back from the implementation.
#[test]
fn the_lsb_matches_the_bit_depth() {
    let d16 = DitherState::for_config(&config(Dither::Triangular, BitDepth::Int16));
    let d24 = DitherState::for_config(&config(Dither::Triangular, BitDepth::Int24));
    assert_eq!(d16.lsb, Some(1.0 / 32768.0));
    assert_eq!(d24.lsb, Some(1.0 / 8388608.0));
}
