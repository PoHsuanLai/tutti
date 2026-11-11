//! Clipping is counted, and a normalized export leaves an integer format its
//! headroom so that it does not clip.

use tutti_export::{
    Dither, EncodeConfig, ExportConfig, Format, FrozenClock, Normalize, Pcm, RenderConfig,
    RenderGraph, render_normalized_to_file, render_to_file,
};
use tutti_graph::GraphBuilder;
use tutti_nodes::testing::{Const, Osc};
use tutti_types::Seconds;
use tutti_types::{ChannelLayout, Db, Hz, SampleRate};

const RATE: SampleRate = SampleRate(48_000.0);

fn config(pcm: Pcm, dither: Dither) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: RATE,
            duration_seconds: Seconds(0.1),
            ..Default::default()
        },
        encode: EncodeConfig {
            format: Format::Wav(pcm),
            channels: ChannelLayout::STEREO,
            topology: None,
        },
        dither,
        ..Default::default()
    }
}

fn built(g: GraphBuilder) -> RenderGraph {
    let (editor, executor) = g.build(RenderGraph::prepare(RATE)).expect("builds");
    RenderGraph::new(editor, executor).expect("paired")
}

fn dc(value: f32) -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let id = g.add(Const::frame(&[value, value]));
    g.pipe_output(id);
    built(g)
}

/// A sine at a quarter of the rate, whose samples are 0, 1, 0, -1: its sample
/// peak is its true peak, so a normalization to 0 dBTP puts its crests exactly
/// on full scale.
fn quarter_rate_sine() -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let id = g.add(Osc::sine(Hz(RATE.get() as f32 / 4.0)));
    g.pipe_output(id);
    built(g)
}

/// `Written::clipped_samples` is the number of samples the quantizer clamped:
/// every sample of a 1.2 DC (two channels, 4,800 frames) at 16 bit, none of a
/// 1.0 (full scale is representable), and none for a format that does not
/// quantize.
///
/// Mutation (run): count `> 1.0` as `>= 1.0` → the 1.0 render counts every
/// sample → fails. Mutation (run): drop the `clipped +=` in `encode_to_file` →
/// 0 for the 1.2 render → fails. Mutation (run): count for `Float32` too →
/// the float render reports 9,600 → fails.
#[test]
fn the_samples_the_quantizer_clamps_are_counted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("clip.wav");
    let frames = 4_800u64;

    let over = render_to_file(
        dc(1.2),
        &config(Pcm::Int16, Dither::Off),
        &FrozenClock,
        &path,
    )
    .unwrap();
    assert_eq!(over.clipped_samples, 2 * frames);

    let at = render_to_file(
        dc(1.0),
        &config(Pcm::Int16, Dither::Off),
        &FrozenClock,
        &path,
    )
    .unwrap();
    assert_eq!(at.clipped_samples, 0, "full scale is not clipping");

    let float = render_to_file(
        dc(1.2),
        &config(Pcm::Float32, Dither::Off),
        &FrozenClock,
        &path,
    )
    .unwrap();
    assert_eq!(float.clipped_samples, 0, "a float file stores 1.2 as it is");
}

/// The largest absolute code in a 16- or 24-bit WAV.
fn max_code(path: &std::path::Path) -> i64 {
    let reader = hound::WavReader::open(path).unwrap();
    reader
        .into_samples::<i32>()
        .map(|s| i64::from(s.unwrap()).abs())
        .max()
        .unwrap()
}

/// A peak normalized to 0 dBTP at an integer depth, with dither, clips nothing
/// and never reaches the saturating code: the target is pulled down to one LSB
/// plus the dither amplitude below full scale (1, 1.5 or 2 LSB), so the crest
/// plus its dither stays at or under `full - 1` (for one LSB of headroom with
/// triangular dither, a crest plus noise reaches `full`). Every depth and
/// dither. The target the gain lands the measured peak on is the clamped figure.
///
/// Mutation (run): `Normalize::within` returning `self` (the 0 dBTP target
/// kept) → about half the crests carry positive dither past full scale and are
/// counted, and the landed target reads 0 → fails. Mutation (run): the dither term
/// dropped from `headroom_ceiling` (1 LSB for every dither) → triangular
/// crests reach the saturating code and the reported target is off → fails.
#[test]
fn a_zero_dbtp_target_leaves_an_integer_format_its_headroom() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zero.wav");
    for (pcm, bits) in [(Pcm::Int16, 16u32), (Pcm::Int24, 24)] {
        for (dither, lsbs) in [
            (Dither::Off, 1.0f64),
            (Dither::Rectangular, 1.5),
            (Dither::Triangular, 2.0),
        ] {
            let written = render_normalized_to_file(
                quarter_rate_sine(),
                &config(pcm, dither),
                &FrozenClock,
                Normalize::peak(Db(0.0)),
                &path,
            )
            .unwrap();
            assert_eq!(written.clipped_samples, 0, "{pcm:?} {dither:?}");
            let full = (1i64 << (bits - 1)) - 1;
            assert!(
                max_code(&path) < full,
                "{pcm:?} {dither:?}: reached the saturating code"
            );
            let want = (20.0 * (1.0 - lsbs / f64::from(1u32 << (bits - 1))).log10()) as f32;
            // The effective target is where the measured peak lands under the
            // reported gain: `gain` is `target - measured.true_peak` exactly.
            let gain = written.gain.expect("a normalizing export reports");
            let measured = written.measured.expect("and what it measured");
            let landed = measured.true_peak.get() + gain.get();
            assert!(
                (landed - want).abs() < 1e-5,
                "{pcm:?} {dither:?}: target {landed} vs {want}"
            );
        }
    }
}

/// Non-finite samples are counted as clipped: a NaN is written as 0 and an
/// infinity as full scale, and neither is the signal that was rendered.
///
/// Mutation (run): `clipped_in` back to `s.abs() > 1.0` → the NaN render
/// counts 0 → fails.
#[test]
fn non_finite_samples_are_counted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nan.wav");
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let written = render_to_file(
            dc(value),
            &config(Pcm::Int16, Dither::Off),
            &FrozenClock,
            &path,
        )
        .unwrap();
        assert_eq!(written.clipped_samples, 2 * 4_800, "{value}");
    }
}
