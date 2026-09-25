//! Dither, measured end to end through a real encoder.
//!
//! The unit tests in `process/dither.rs` check the *shape* of the stage — that
//! `Float32` is skipped, that the LSB matches the depth, that noise stays within
//! one LSB. What none of them ask is whether the two modes actually differ, or
//! whether each has the distribution its name claims. `Rectangular` and
//! `Triangular` are distinguishable only by their statistics, so a mode that
//! silently fell through to the other would pass every existing test.
//!
//! # Why this is deterministic
//!
//! The RNG is a fixed-seed xorshift32 (`0x12345678`, `dither.rs:41`) advanced
//! continuously across blocks and channels. Same config in, same file out — so
//! these assertions are exact and reproducible, not sampled with a flaky margin.
//! That also means the seed is load-bearing: reseeding per block would correlate
//! the noise to the block grid, which is the bug the continuous state prevents.
//!
//! # Measuring against DC
//!
//! Every case renders a constant. Quantized without dither, a constant maps to
//! one integer and the file is perfectly flat — so anything non-flat in the
//! output *is* the dither, and the noise can be measured directly by subtracting
//! the level rather than modelled.
//!
//! Gated on `wav`, matching `surround_export.rs`: every case writes a WAV and
//! reads it back through `hound`, which this crate only links under that
//! feature.

#![cfg(feature = "wav")]

use tutti_export::{
    render_to_file, AudioFormat, BitDepth, ChannelLayout, Dither, EncodeConfig, ExportConfig,
    FrozenClock, RenderConfig, RenderGraph,
};
use tutti_graph::GraphBuilder;
use tutti_nodes::testing::Const;

const SR: f64 = 44_100.0;
/// One LSB at 16-bit in the [-1, 1] float domain.
const LSB16: f32 = 1.0 / 32_767.0;

/// `g`, built for an export at [`SR`] — the rate every config here
/// renders at (a graph prepared at another is refused, not re-rated).
fn built(g: GraphBuilder) -> RenderGraph {
    let (editor, executor) = g
        .build(RenderGraph::prepare(tutti_core::SampleRate(SR)))
        .expect("builds");
    RenderGraph::new(editor, executor).expect("built together")
}

fn dc_graph(level: f32) -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let id = g.add_unit(Box::new(Const::frame(&[level, level])));
    g.pipe_output(id);
    built(g)
}

fn config(dither: Dither, bit_depth: BitDepth) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: tutti_core::SampleRate(SR),
            duration_seconds: 0.5,
            ..Default::default()
        },
        encode: EncodeConfig {
            format: AudioFormat::Wav,
            bit_depth,
            channels: ChannelLayout::STEREO,
        },
        dither,
        ..Default::default()
    }
}

/// Render a DC level to a 16-bit WAV and return the samples as integers.
///
/// Integers, not floats: dither is defined in LSBs, and comparing in the
/// quantized domain is what makes "within one LSB" an exact statement rather
/// than a float-tolerance argument.
fn render_i16(dither: Dither, level: f32, path: &std::path::Path) -> Vec<i32> {
    render_to_file(
        dc_graph(level),
        &config(dither, BitDepth::Int16),
        &FrozenClock,
        path,
    )
    .unwrap();
    hound::WavReader::open(path)
        .expect("open wav")
        .into_samples::<i32>()
        .map(|s| s.expect("read sample"))
        .collect()
}

/// With dither off, a constant quantizes to a single integer — every time.
///
/// The control. If this is not flat, the "anything non-flat is dither" reasoning
/// the rest of the file rests on does not hold.
#[test]
fn dither_off_leaves_a_constant_perfectly_flat() {
    let d = tempfile::tempdir().unwrap();
    let samples = render_i16(Dither::Off, 0.25, &d.path().join("off.wav"));

    let first = samples[0];
    assert!(
        samples.iter().all(|&s| s == first),
        "undithered DC must be one constant integer; found {} distinct values",
        {
            let mut v: Vec<i32> = samples.clone();
            v.sort_unstable();
            v.dedup();
            v.len()
        }
    );
    assert_eq!(
        first,
        tutti_core::pcm::f32_to_i16(0.25) as i32,
        "undithered quantization must match the engine's canonical converter"
    );
}

/// Both dither modes actually perturb the signal.
///
/// A mode wired to a no-op would leave the file flat and be invisible to every
/// existing test, since "flat" is also what correct undithered output looks like.
#[test]
fn both_dither_modes_perturb_a_constant() {
    let d = tempfile::tempdir().unwrap();

    for mode in [Dither::Rectangular, Dither::Triangular] {
        let samples = render_i16(mode, 0.25, &d.path().join(format!("{mode:?}.wav")));
        let distinct = {
            let mut v = samples.clone();
            v.sort_unstable();
            v.dedup();
            v.len()
        };
        assert!(
            distinct > 1,
            "{mode:?} left the signal flat ({distinct} distinct value) — \
             the mode is not reaching the quantizer"
        );
    }
}

/// The two modes are genuinely different, not aliases.
///
/// Rectangular draws once per sample; triangular draws twice and subtracts. If
/// the match arm for one fell through to the other, every other assertion here
/// would still pass — the output would be noisy, bounded, and centred. Only a
/// direct comparison catches it.
#[test]
fn rectangular_and_triangular_are_not_the_same_mode() {
    let d = tempfile::tempdir().unwrap();
    let rect = render_i16(Dither::Rectangular, 0.25, &d.path().join("r.wav"));
    let tri = render_i16(Dither::Triangular, 0.25, &d.path().join("t.wav"));

    assert_eq!(rect.len(), tri.len(), "same config, same length");
    assert!(
        rect != tri,
        "Rectangular and Triangular produced byte-identical output — \
         one mode is falling through to the other"
    );
}

/// Each mode's noise spans the width its distribution implies.
///
/// Rectangular is one draw in [-0.5, 0.5] LSB, so quantized output touches at
/// most two adjacent integers. Triangular is the difference of two draws, in
/// [-1, 1] LSB, so it reaches three. That difference is the *point* of TPDF —
/// it costs 3 dB of noise floor to buy freedom from noise modulation — and it is
/// the cheapest way to tell the two apart from the output alone.
#[test]
fn each_mode_spans_the_width_its_distribution_implies() {
    let d = tempfile::tempdir().unwrap();

    for (mode, max_span) in [(Dither::Rectangular, 2), (Dither::Triangular, 3)] {
        let samples = render_i16(mode, 0.25, &d.path().join(format!("span_{mode:?}.wav")));
        let lo = *samples.iter().min().unwrap();
        let hi = *samples.iter().max().unwrap();
        let span = hi - lo + 1;

        assert!(
            span <= max_span,
            "{mode:?} spans {span} integers ({lo}..={hi}), expected at most {max_span} \
             — noise wider than the distribution allows"
        );
        // Over 44100 frames the tails are certain to be hit, so a narrower span
        // means the noise is not the distribution it claims.
        assert_eq!(
            span, max_span,
            "{mode:?} spans only {span} integers ({lo}..={hi}), expected {max_span}"
        );
    }
}

/// Dither is zero-mean: it must not shift the signal it is applied to.
///
/// A biased dither is a DC offset — audible as a click at the start and end of
/// every export, and cumulative through a mastering chain. `rectangular_noise`
/// subtracts 0.5 for exactly this reason, and `triangular_noise` is a difference
/// of two identically-distributed draws.
#[test]
fn dither_does_not_bias_the_signal() {
    let d = tempfile::tempdir().unwrap();
    let level = 0.25f32;
    let exact = level * 32_767.0; // the unquantized target, in integer units

    for mode in [Dither::Rectangular, Dither::Triangular] {
        let samples = render_i16(mode, level, &d.path().join(format!("bias_{mode:?}.wav")));
        let mean = samples.iter().map(|&s| s as f64).sum::<f64>() / samples.len() as f64;

        // Well inside one LSB over 88200 samples. A mode that forgot to centre
        // its noise would sit ~0.5 LSB off.
        assert!(
            (mean - exact as f64).abs() < 0.1,
            "{mode:?}: mean {mean:.4} is off the target {exact:.4} by \
             {:.4} LSB — the noise is not zero-mean",
            (mean - exact as f64).abs()
        );
    }
}

/// Triangular noise has the higher variance of the two.
///
/// Rectangular is uniform on [-0.5, 0.5] LSB, variance 1/12; triangular is the
/// difference of two such uniforms, variance 1/6 — twice as much, the 3 dB TPDF
/// costs.
///
/// # The measured ratio is ~1.34, not ~2, and that is correct
///
/// This test first asserted `~2x` and failed at 1.32. The expectation was wrong,
/// not the code. The 2x holds for the noise *before* it is quantized; what a
/// file can be measured on is the noise *after* rounding to integers. Since both
/// dithers are sub-LSB, rounding dominates the result and compresses the ratio.
/// An independent NumPy model of ideal rect/tri dither at this level gives 2.010
/// pre-quantization and 1.342 post — matching tutti's 1.321 to within sampling
/// noise, with per-mode variances (0.187, 0.251) against tutti's (0.188, 0.248).
///
/// So the bound below brackets the *post-quantization* ratio. Widening it to
/// admit 2.0 would be asserting something the observable signal cannot show.
#[test]
fn triangular_noise_carries_more_variance_than_rectangular() {
    let d = tempfile::tempdir().unwrap();
    let level = 0.25f32;
    let exact = (level * 32_767.0) as f64;

    let variance = |samples: &[i32]| -> f64 {
        let n = samples.len() as f64;
        samples
            .iter()
            .map(|&s| (s as f64 - exact).powi(2))
            .sum::<f64>()
            / n
    };

    let rect = variance(&render_i16(
        Dither::Rectangular,
        level,
        &d.path().join("vr.wav"),
    ));
    let tri = variance(&render_i16(
        Dither::Triangular,
        level,
        &d.path().join("vt.wav"),
    ));

    let ratio = tri / rect;
    // Brackets the post-quantization ratio (~1.34 by the NumPy model in the doc
    // comment above), loose enough not to be brittle but tight enough that a
    // mode falling through to the other — ratio 1.0 — fails.
    assert!(
        (1.15..=1.6).contains(&ratio),
        "triangular/rectangular variance ratio is {ratio:.3} \
         (rect {rect:.4}, tri {tri:.4}); expected ~1.34 after quantization \
         (~2x before it). A ratio near 1.0 means both modes are the same noise."
    );
}

/// Float output is never dithered, at any mode.
///
/// Dither decorrelates *quantization* error and a float export does not
/// quantize. Pinned end to end because the unit test covers the state object,
/// not the file: a stage added later that dithered before the float write would
/// leave that test green.
#[test]
fn float32_output_is_never_dithered_at_any_mode() {
    let d = tempfile::tempdir().unwrap();
    let level = 0.25f32;

    for mode in [Dither::Off, Dither::Rectangular, Dither::Triangular] {
        let path = d.path().join(format!("f32_{mode:?}.wav"));
        render_to_file(
            dc_graph(level),
            &config(mode, BitDepth::Float32),
            &FrozenClock,
            &path,
        )
        .unwrap();

        let samples: Vec<f32> = hound::WavReader::open(&path)
            .unwrap()
            .into_samples::<f32>()
            .map(|s| s.unwrap())
            .collect();

        assert!(
            samples.iter().all(|&s| s == level),
            "{mode:?}: float output must be bit-exact, found {} != {level}",
            samples.iter().find(|&&s| s != level).unwrap()
        );
    }
}

/// The noise stays inside one LSB of the depth actually being written.
///
/// At 24-bit the LSB is 256x smaller than at 16-bit, so a dither that computed
/// its step from a hardcoded depth would be wildly too loud here — audible, and
/// invisible to a test that only looks at 16-bit.
#[test]
fn noise_scales_with_the_target_bit_depth() {
    let d = tempfile::tempdir().unwrap();
    let level = 0.25f32;
    let path = d.path().join("d24.wav");

    render_to_file(
        dc_graph(level),
        &config(Dither::Triangular, BitDepth::Int24),
        &FrozenClock,
        &path,
    )
    .unwrap();

    let samples: Vec<i32> = hound::WavReader::open(&path)
        .unwrap()
        .into_samples::<i32>()
        .map(|s| s.unwrap())
        .collect();

    let exact = level * 8_388_607.0;
    let max_dev = samples
        .iter()
        .map(|&s| (s as f32 - exact).abs())
        .fold(0.0f32, f32::max);

    // One LSB of *24-bit* is 1 integer step here. Two steps covers TPDF's
    // ±1 LSB plus the half-step of quantization itself.
    assert!(
        max_dev <= 2.0,
        "24-bit dither deviates by {max_dev} integer steps; \
         at 24-bit one LSB is 1 step, so this noise is scaled to the wrong depth"
    );

    // And it must not be so small it vanished — that would mean no dither.
    assert!(
        samples.iter().any(|&s| s as f32 != exact.round()),
        "24-bit output is perfectly flat; the dither never reached it"
    );

    // Sanity: the 16-bit LSB is 256x coarser, so the same noise expressed in
    // float terms must be far below it.
    assert!(
        max_dev / 8_388_607.0 < LSB16,
        "24-bit dither noise is as loud as a 16-bit LSB — wrong depth"
    );
}
