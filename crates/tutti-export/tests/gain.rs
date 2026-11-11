//! `Written::gain` and `Written::measured`: what a normalizing export reports,
//! pinned against figures worked out from the signal, not against another
//! render.

mod support;

use crate::support::{render_normalized_to_file, render_to_file};
use tutti_export::{
    Dither, EncodeConfig, ExportConfig, Format, FrozenClock, Normalize, Pcm, RenderConfig,
    RenderGraph,
};
use tutti_graph::GraphBuilder;
use tutti_measure::{LoudnessConfig, measure_loudness};
use tutti_nodes::testing::Osc;
use tutti_types::{ChannelLayout, Db, Hz, Interleaved, SampleRate, Seconds};

const RATE: SampleRate = SampleRate(48_000.0);

fn config(pcm: Pcm, dither: Dither, seconds: f64) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: RATE,
            duration_seconds: Seconds(seconds),
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

/// A full-scale sine in both channels (a mono node feeds every output).
fn sine(frequency: Hz) -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let id = g.add(Osc::sine(frequency));
    g.pipe_output(id);
    let (editor, executor) = g.build(RenderGraph::prepare(RATE)).expect("builds");
    RenderGraph::new(editor, executor).expect("paired")
}

/// A peak normalization reports the gain it applied and the reading it came
/// from. The quarter-rate sine's samples are 0, 1, 0, -1, so its true peak is
/// 0 dBTP, and the gain to -6 dBTP is -6 dB; a plain export reports neither.
///
/// Mutation (run, each fails this test): `written.gain = None` in
/// `render_normalized_to_file`; `written.gain = Some(Db(0.0))`;
/// `written.measured = None`; `encode_to_file` reporting `gain: Some(Db(0.0))`
/// for a plain export (the `None` assertions fail).
#[test]
fn a_peak_normalized_export_reports_its_gain_and_reading() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("six.wav");
    for target in [-6.0f32, -3.0] {
        let written = render_normalized_to_file(
            sine(Hz(RATE.get() as f32 / 4.0)),
            &config(Pcm::Int16, Dither::Triangular, 0.1),
            &FrozenClock,
            Normalize::peak(Db(target)),
            &path,
        )
        .unwrap();
        let gain = written.gain.expect("a normalizing export reports its gain");
        let measured = written.measured.expect("and the reading it came from");
        assert!((gain.get() - target).abs() < 0.05, "{target}: {gain:?}");
        assert!(measured.true_peak.get().abs() < 0.05, "{measured:?}");
    }

    let plain = render_to_file(
        sine(Hz(1_000.0)),
        &config(Pcm::Int16, Dither::Off, 0.1),
        &FrozenClock,
        &path,
    )
    .unwrap();
    assert_eq!(plain.gain, None);
    assert_eq!(plain.measured, None);
}

/// A loudness normalization of a known level lands on its target. A 1 kHz
/// sine at full scale in both channels reads 0 LUFS (EBU Tech 3341's
/// calibration: -0.691 + 10 log10 of the channel sum, K-weighting being unity
/// at 1 kHz, to within 0.1 LU), so the gain to -23 LUFS is -23 dB within the
/// same tolerance, and the file written with it reads -23 LUFS.
///
/// Mutation (run, each fails this test): `Normalize::gain_for` calling
/// `gain_to(ceiling, ceiling)` (the gain reads -1 dB); `written.gain = None`;
/// measuring after `apply_gain` and reporting that reading (it reads -23, not
/// 0).
#[test]
fn a_loudness_normalized_export_reports_the_gain_to_its_target() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lufs.wav");
    let written = render_normalized_to_file(
        sine(Hz(1_000.0)),
        &config(Pcm::Float32, Dither::Off, 3.0),
        &FrozenClock,
        Normalize::lufs(Db(-23.0)),
        &path,
    )
    .unwrap();
    let measured = written.measured.expect("reported");
    let gain = written.gain.expect("reported");
    assert!(measured.lufs.get().abs() < 0.1, "{measured:?}");
    assert!((gain.get() + 23.0).abs() < 0.1, "{gain:?}");

    let samples: Vec<f32> = hound::WavReader::open(&path)
        .unwrap()
        .into_samples::<f32>()
        .map(Result::unwrap)
        .collect();
    let meter = LoudnessConfig::new(RATE, ChannelLayout::STEREO);
    let written_loudness =
        measure_loudness(&meter, Interleaved::new(&samples, ChannelLayout::STEREO)).unwrap();
    assert!(
        (written_loudness.lufs.get() + 23.0).abs() < 0.1,
        "{written_loudness:?}"
    );
}
