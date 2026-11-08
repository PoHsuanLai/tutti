//! Clipping is counted, and a normalized export leaves an integer format its
//! headroom so that it does not clip.

use tutti_export::{
    Dither, EncodeConfig, ExportConfig, Format, FrozenClock, Normalize, Pcm, RenderConfig,
    RenderGraph, render_normalized_to_file, render_to_file,
};
use tutti_graph::GraphBuilder;
use tutti_nodes::testing::{Const, Osc};
use tutti_types::{ChannelLayout, Db, Hz, SampleRate};

const RATE: SampleRate = SampleRate(48_000.0);

fn config(pcm: Pcm, dither: Dither) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: RATE,
            duration_seconds: 0.1,
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

/// A peak normalized to 0 dBTP at an integer depth, with dither, clips nothing:
/// the target is pulled down to one LSB plus the dither amplitude below full
/// scale. At the full-scale 24-bit and 16-bit depths, every dither.
///
/// Mutation (run): `Normalize::within` returning `self` (the 0 dBTP target
/// kept) → about half the crests carry positive dither past full scale and are
/// counted → fails.
#[test]
fn a_zero_dbtp_target_leaves_an_integer_format_its_headroom() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zero.wav");
    for pcm in [Pcm::Int16, Pcm::Int24] {
        for dither in [Dither::Off, Dither::Rectangular, Dither::Triangular] {
            let written = render_normalized_to_file(
                quarter_rate_sine(),
                &config(pcm, dither),
                &FrozenClock,
                Normalize::peak(Db(0.0)),
                &path,
            )
            .unwrap();
            assert_eq!(written.clipped_samples, 0, "{pcm:?} {dither:?}");
        }
    }
}
