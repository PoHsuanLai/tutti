//! Stopping a render from its tick, and what that leaves behind.
//!
//! No hardware and no sleeps: the tick counts blocks and answers `Stop`.
//! The graph renders in blocks of `GRAPH_MAX_BLOCK` frames, so a render of
//! `BLOCKS` of them ticks `BLOCKS` times per pass.

#![cfg(all(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]

use tutti_export::{
    ChannelLayout, EncodeConfig, ExportConfig, FlacLevel, Flow, Format, FrozenClock,
    GRAPH_MAX_BLOCK, IntDepth, Normalize, Outcome, Pass, Pcm, Progress, Quality, RenderConfig,
    RenderGraph, render_normalized_to_file_with, render_to_buffers_with, render_to_file_with,
};
use tutti_graph::GraphBuilder;
use tutti_nodes::testing::Const;
use tutti_types::Seconds;
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

/// FLAC at 24 bit and the default level.
fn flac() -> Format {
    Format::Flac {
        depth: IntDepth::Int24,
        level: FlacLevel::default(),
    }
}

/// Ogg Vorbis at the default quality.
fn vorbis() -> Format {
    Format::Vorbis {
        quality: Quality::default(),
    }
}

fn config(format: Format) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: RATE,
            duration_seconds: Seconds((BLOCKS * GRAPH_MAX_BLOCK.get()) as f64 / RATE.get()),
            ..Default::default()
        },
        encode: EncodeConfig {
            format,
            channels: ChannelLayout::STEREO,
            topology: None,
        },
        ..Default::default()
    }
}

fn formats() -> [(Format, &'static str); 4] {
    [
        (Format::Wav(Pcm::Int16), "wav"),
        (flac(), "flac"),
        (vorbis(), "ogg"),
        (Format::Aiff(Pcm::Int16), "aiff"),
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
/// Mutation (run): `encode_to_file` writing to `path` itself -> the partial
/// file stays and the `exists` assertion fails.
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

/// Every name in `dir`.
fn names(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// A cancel never touches a file already at the path, whichever pass it
/// lands in, and leaves no temporary beside it: the render is written aside
/// and renamed over the target only when it finishes.
///
/// Mutation (run): `encode_to_file` writing to `path` itself (no `Staged`) ->
/// the old bytes are truncated and the comparison fails in every case.
/// Mutation (run): `Staged::drop` not removing the temporary -> the
/// directory listing holds a `.tmp` and fails.
#[test]
fn a_cancel_leaves_the_file_it_would_have_replaced() {
    for (format, ext) in formats() {
        let dir = tempfile::tempdir().unwrap();
        let name = format!("old.{ext}");
        let path = dir.path().join(&name);
        let old = b"an earlier export, byte for byte".to_vec();
        for (normalize, pass) in [
            (None, 0u8),
            (Some(Normalize::peak(Db(-6.0))), 0),
            (Some(Normalize::peak(Db(-6.0))), 1),
        ] {
            std::fs::write(&path, &old).unwrap();
            let mut seen = Vec::new();
            let outcome = match normalize {
                None => render_to_file_with(
                    graph(),
                    &config(format),
                    &FrozenClock,
                    &path,
                    &mut stop_after(2, pass, &mut seen),
                ),
                Some(n) => render_normalized_to_file_with(
                    graph(),
                    &config(format),
                    &FrozenClock,
                    n,
                    &path,
                    &mut stop_after(2, pass, &mut seen),
                ),
            }
            .unwrap();
            assert_eq!(outcome, Outcome::Cancelled, "{ext} pass {pass}");
            assert_eq!(std::fs::read(&path).unwrap(), old, "{ext} pass {pass}");
            assert_eq!(
                names(dir.path()),
                std::slice::from_ref(&name),
                "{ext} pass {pass}"
            );
        }
    }
}

/// A render that finishes replaces the old file with the complete new one,
/// and nothing else is left in the directory.
#[test]
fn a_finished_render_replaces_the_old_file() {
    for (format, ext) in formats() {
        let dir = tempfile::tempdir().unwrap();
        let name = format!("r.{ext}");
        let path = dir.path().join(&name);
        std::fs::write(&path, b"old").unwrap();
        let out = render_to_file_with(graph(), &config(format), &FrozenClock, &path, &mut |_| {
            Flow::Continue
        })
        .unwrap();
        let Outcome::Finished(written) = out else {
            panic!("{ext} cancelled")
        };
        assert_eq!(written.path, path);
        assert!(written.bytes > 3 && std::fs::metadata(&path).unwrap().len() == written.bytes);
        assert_eq!(names(dir.path()), [name], "{ext}");
    }
}

/// A render that fails after its temporary exists (here the rename: the
/// target is a directory) is an error, the target is untouched and the
/// temporary is gone. A target in a directory that cannot hold the file
/// still errors.
///
/// Mutation (run): `Staged::commit` marking the file kept before the rename
/// -> the temporary stays behind; fails.
#[test]
fn a_failed_write_leaves_the_target_and_no_temporary() {
    for (format, ext) in formats() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(format!("d.{ext}"));
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("keep"), b"x").unwrap();
        let err = render_to_file_with(graph(), &config(format), &FrozenClock, &target, &mut |_| {
            Flow::Continue
        })
        .expect_err("a directory cannot be replaced by a file");
        assert!(matches!(err, tutti_export::Error::Io(_)), "{ext}: {err}");
        assert!(target.join("keep").exists(), "{ext}");
        assert_eq!(names(dir.path()), [format!("d.{ext}")], "{ext}");

        let missing = dir.path().join("no-such-dir").join(format!("m.{ext}"));
        let err = render_to_file_with(
            graph(),
            &config(format),
            &FrozenClock,
            &missing,
            &mut |_| Flow::Continue,
        )
        .expect_err("the directory does not exist");
        assert!(matches!(err, tutti_export::Error::Io(_)), "{ext}: {err}");
    }
}

/// Rendering to memory stops the same way, with nothing returned.
#[test]
fn a_stopped_buffer_render_is_cancelled() {
    let mut seen = Vec::new();
    let outcome = render_to_buffers_with(
        graph(),
        &config(Format::Wav(Pcm::Int16)),
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
        &config(Format::Wav(Pcm::Int16)),
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
        &config(Format::Wav(Pcm::Int16)),
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
    let cfg = config(Format::Wav(Pcm::Int16));
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

/// `Stop` on the final block's tick is `Cancelled` (the tick runs before the
/// render is known to be complete), and the file is not delivered.
///
/// Mutation (run): `drive` skipping the tick after the last block -> the
/// outcome is `Finished`; fails.
#[test]
fn a_stop_on_the_final_tick_is_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("last.wav");
    let outcome = render_to_file_with(
        graph(),
        &config(Format::Wav(Pcm::Int16)),
        &FrozenClock,
        &path,
        &mut |p| {
            if p.produced == p.total {
                Flow::Stop
            } else {
                Flow::Continue
            }
        },
    )
    .unwrap();
    assert_eq!(outcome, Outcome::Cancelled);
    assert!(!path.exists());
}
