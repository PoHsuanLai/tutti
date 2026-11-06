//! Stopping a render from its tick, and what that leaves behind.
//!
//! No hardware and no sleeps: the tick counts blocks and answers `Stop`.
//! The graph renders in blocks of `GRAPH_MAX_BLOCK` frames, so a render of
//! `BLOCKS` of them ticks `BLOCKS` times per pass.

#![cfg(all(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]

use tutti_export::{
    AudioFormat, BitDepth, ChannelLayout, EncodeConfig, ExportConfig, Flow, FrozenClock,
    GRAPH_MAX_BLOCK, Normalize, Outcome, Pass, Progress, RenderConfig, RenderGraph,
    render_normalized_to_file_with, render_to_buffers_with, render_to_file_with,
};
use tutti_graph::GraphBuilder;
use tutti_nodes::testing::Const;
use tutti_types::{Db, SampleRate, Samples};

const RATE: SampleRate = SampleRate(48_000.0);
const BLOCKS: usize = 12;

fn graph() -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let id = g.add(Const::frame(&[0.25, 0.25]));
    g.pipe_output(id);
    let (editor, executor) = g.build(RenderGraph::prepare(RATE)).expect("builds");
    RenderGraph::new(editor, executor).expect("built together")
}

fn config(format: AudioFormat) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: RATE,
            duration_seconds: (BLOCKS * GRAPH_MAX_BLOCK.get()) as f64 / RATE.get(),
            ..Default::default()
        },
        encode: EncodeConfig {
            format,
            bit_depth: BitDepth::Int16,
            channels: ChannelLayout::STEREO,
            topology: None,
        },
        ..Default::default()
    }
}

fn formats() -> [(AudioFormat, &'static str); 4] {
    [
        (AudioFormat::Wav, "wav"),
        (AudioFormat::Flac(Default::default()), "flac"),
        (AudioFormat::OggVorbis(Default::default()), "ogg"),
        (AudioFormat::Aiff, "aiff"),
    ]
}

/// A tick that stops once it has been told of `n` blocks of `pass`.
fn stop_after(n: usize, pass: u8, seen: &mut Vec<Progress>) -> impl FnMut(Progress) -> Flow + '_ {
    move |p| {
        seen.push(p);
        let in_pass = seen.iter().filter(|q| q.pass.index() == pass).count();
        if p.pass.index() == pass && in_pass >= n {
            Flow::Stop
        } else {
            Flow::Continue
        }
    }
}

/// A render stopped after N blocks is `Cancelled` (not an error, not a short
/// file), has rendered exactly N blocks, and leaves nothing at its path, in
/// every format.
///
/// Mutation (run): `encode_to_file` not removing the file on `Cancelled`
/// -> the `exists` assertion fails.
/// Mutation (run): `drive` ignoring the tick's answer -> the outcome
/// assertion fails.
#[test]
fn a_stopped_file_render_is_cancelled_and_leaves_no_file() {
    let dir = tempfile::tempdir().unwrap();
    for (format, ext) in formats() {
        let path = dir.path().join(format!("a.{ext}"));
        let mut seen = Vec::new();
        let outcome = render_to_file_with(
            graph(),
            &config(format),
            &FrozenClock,
            &path,
            &mut stop_after(3, 0, &mut seen),
        )
        .unwrap_or_else(|e| panic!("{ext}: {e}"));
        assert_eq!(outcome, Outcome::Cancelled, "{ext}");
        assert_eq!(seen.len(), 3, "{ext}: stopped at the third block");
        assert!(!path.exists(), "{ext}: a cancelled export left a file");
    }
}

/// A file already at the path is replaced by the render's, so a cancelled
/// export removes it too: the encoder truncated it on create.
///
/// Mutation (run): `encode_to_file` not removing the file on `Cancelled` ->
/// the truncated file remains and `exists` fails.
#[test]
fn a_cancelled_export_does_not_leave_the_file_it_overwrote() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.wav");
    std::fs::write(&path, b"an earlier export").unwrap();
    let mut seen = Vec::new();
    let outcome = render_to_file_with(
        graph(),
        &config(AudioFormat::Wav),
        &FrozenClock,
        &path,
        &mut stop_after(1, 0, &mut seen),
    )
    .unwrap();
    assert_eq!(outcome, Outcome::Cancelled);
    assert!(!path.exists());
}

/// Rendering to memory stops the same way, with nothing returned.
#[test]
fn a_stopped_buffer_render_is_cancelled() {
    let mut seen = Vec::new();
    let outcome = render_to_buffers_with(
        graph(),
        &config(AudioFormat::Wav),
        &FrozenClock,
        &mut stop_after(2, 0, &mut seen),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Cancelled));
    assert_eq!(seen.len(), 2);
}

/// Normalize is two passes, and stopping in the second (the write) cancels
/// the export and removes the file, after the whole render has run.
///
/// Mutation (run): the second pass given a tick that never stops instead of
/// the host's -> the outcome is `Finished` and the file exists; fails.
#[test]
fn the_second_normalize_pass_stops() {
    let dir = tempfile::tempdir().unwrap();
    for (format, ext) in formats() {
        let path = dir.path().join(format!("n.{ext}"));
        let mut seen = Vec::new();
        let outcome = render_normalized_to_file_with(
            graph(),
            &config(format),
            &FrozenClock,
            Normalize::peak(Db(-6.0)),
            &path,
            &mut stop_after(2, 1, &mut seen),
        )
        .unwrap_or_else(|e| panic!("{ext}: {e}"));
        assert_eq!(outcome, Outcome::Cancelled, "{ext}");
        let first = seen.iter().filter(|p| p.pass.index() == 0).count();
        let second = seen.iter().filter(|p| p.pass.index() == 1).count();
        assert_eq!(first, BLOCKS, "{ext}: the render pass ran to its end");
        assert_eq!(second, 2, "{ext}");
        assert!(!path.exists(), "{ext}");
    }
}

/// Stopping in the first pass never starts the second.
#[test]
fn stopping_the_first_normalize_pass_never_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("n.wav");
    let mut seen = Vec::new();
    let outcome = render_normalized_to_file_with(
        graph(),
        &config(AudioFormat::Wav),
        &FrozenClock,
        Normalize::peak(Db(-6.0)),
        &path,
        &mut stop_after(4, 0, &mut seen),
    )
    .unwrap();
    assert_eq!(outcome, Outcome::Cancelled);
    assert!(seen.iter().all(|p| p.pass.index() == 0));
    assert!(!path.exists());
}

/// Progress names its pass, rises within each, and ends each at its total;
/// a normalized export reports pass 0 of 2 then pass 1 of 2, and a plain one
/// pass 0 of 1.
///
/// Mutation (run): `Control::report` passing `Samples(0)` as `produced` ->
/// the rising assertion fails. Mutation (run): the second pass reporting as
/// `FIRST_OF_TWO` -> the second-pass assertions fail.
#[test]
fn progress_is_monotonic_per_pass_and_reaches_the_total() {
    let dir = tempfile::tempdir().unwrap();
    let total = Samples(BLOCKS * GRAPH_MAX_BLOCK.get());

    let mut plain = Vec::new();
    let outcome = render_to_file_with(
        graph(),
        &config(AudioFormat::Wav),
        &FrozenClock,
        dir.path().join("p.wav"),
        &mut |p| {
            plain.push(p);
            Flow::Continue
        },
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Finished(_)));
    assert!(
        plain
            .iter()
            .all(|p| p.pass == Pass::ONLY && p.total == total)
    );
    assert!(plain.windows(2).all(|w| w[0].produced < w[1].produced));
    assert_eq!(plain.last().unwrap().produced, total);

    let mut two = Vec::new();
    let cfg = config(AudioFormat::Wav);
    let outcome = render_normalized_to_file_with(
        graph(),
        &cfg,
        &FrozenClock,
        Normalize::peak(Db(-6.0)),
        dir.path().join("n.wav"),
        &mut |p| {
            two.push(p);
            Flow::Continue
        },
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Finished(_)));
    for index in 0..2u8 {
        let pass: Vec<_> = two.iter().filter(|p| p.pass.index() == index).collect();
        assert!(pass.iter().all(|p| p.pass.count() == 2), "pass {index}");
        assert!(pass.windows(2).all(|w| w[0].produced < w[1].produced));
        assert_eq!(pass.last().unwrap().produced, total, "pass {index}");
        assert_eq!(pass.last().unwrap().total, total, "pass {index}");
    }
    // The passes come in order.
    let order: Vec<u8> = two.iter().map(|p| p.pass.index()).collect();
    assert!(order.windows(2).all(|w| w[0] <= w[1]));
}
