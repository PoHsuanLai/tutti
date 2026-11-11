//! Stopping a render, watching it, and what both leave behind (doc 019
//! section 6).
//!
//! No hardware and no sleeps: a progress callback sets a `CancelToken` at a
//! chosen reading, and the render stops at the check that follows. The graph
//! renders in blocks of `GRAPH_MAX_BLOCK` frames; a stage that counts frames
//! reports every 4096 of them, so a render of `BLOCKS` blocks reports every
//! fourth.

#![cfg(all(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]

mod support;
use crate::support::{EncodeConfig, ExportConfig, RenderConfig};

use support::{render_buffers_under, render_file_under, render_to_file};
use tutti_codec::{FlacLevel, Format, IntDepth, Pcm, Quality};
use tutti_export::{
    CancelToken, Control, FileTarget, GRAPH_MAX_BLOCK, Normalize, NormalizeSpec, Outcome, Output,
    Produced, Progress, RenderGraph, Resample, Setup, Stage, Written, render,
};
use tutti_graph::GraphBuilder;
use tutti_nodes::testing::Const;
use tutti_transport::FrozenClock;
use tutti_types::ChannelLayout;
use tutti_types::Seconds;
use tutti_types::{Db, SampleRate, Samples};

const RATE: SampleRate = SampleRate(48_000.0);
/// Blocks in a short render: three reports of 4096 frames.
const BLOCKS: usize = 12;
/// A render of two and a half seconds: three one-second measuring chunks.
const LONG_FRAMES: usize = 120_000;
/// The rate a converting render is written at.
const FILE_RATE: SampleRate = SampleRate(44_100.0);

fn graph() -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let id = g.add(Const::frame(&[0.25, 0.25]));
    g.pipe_output(id);
    let (editor, executor) = g.build(RenderGraph::prepare(RATE)).expect("builds");
    RenderGraph::assemble(editor, executor, Setup::new()).expect("built together")
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

fn config_of(format: Format, frames: usize) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: RATE,
            duration_seconds: Seconds(frames as f64 / RATE.get()),
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

fn config(format: Format) -> ExportConfig {
    config_of(format, BLOCKS * GRAPH_MAX_BLOCK.get())
}

fn formats() -> [(Format, &'static str); 4] {
    [
        (Format::Wav(Pcm::Int16), "wav"),
        (flac(), "flac"),
        (vorbis(), "ogg"),
        (Format::Aiff(Pcm::Int16), "aiff"),
    ]
}

/// What a render reported, and what it returned.
struct Run {
    outcome: Outcome<Written>,
    seen: Vec<Progress>,
}

/// What a render of several outputs reported, and what it returned.
struct RunMany {
    outcome: Outcome<Vec<Produced>>,
    seen: Vec<Progress>,
}

/// The second output of a two-output render: FLAC at 16 bits, so the two files
/// differ in format and quantization.
fn second_format() -> Format {
    Format::Flac {
        depth: IntDepth::Int16,
        level: FlacLevel::default(),
    }
}

/// Renders `config` once into a file at each of `paths` (the first as
/// `config` says, any other as FLAC at 16 bits), normalized when `normalize`
/// is set (the first measures itself and the others take its gain), recording
/// every reading and cancelling the token at the first one `cancel_at`
/// accepts.
fn run_many(
    config: &ExportConfig,
    normalize: Option<Normalize>,
    paths: &[&std::path::Path],
    cancel_at: impl Fn(&Progress) -> bool,
) -> RunMany {
    let token = CancelToken::new();
    let mut seen = Vec::new();
    let mut report = |p: &Progress| {
        seen.push(*p);
        if cancel_at(p) {
            token.cancel();
        }
    };
    let mut control = Control::new(&token).on_progress(&mut report);
    let outputs: Vec<Output> = paths
        .iter()
        .enumerate()
        .map(|(i, path)| {
            let target = if i == 0 {
                config.file_target(path, None)
            } else {
                FileTarget::new(path, second_format(), ChannelLayout::STEREO)
                    .with_resample(config.resample)
            };
            let spec = normalize.map(|n| {
                if i == 0 {
                    NormalizeSpec::own(n)
                } else {
                    NormalizeSpec::from_output(n, 0)
                }
            });
            Output::master(target.with_normalize(spec))
        })
        .collect();
    let outcome = render(
        &mut graph(),
        &config.job().expect("a render"),
        &FrozenClock,
        outputs,
        &mut control,
    )
    .expect("a cancelled or finished render is not an error");
    RunMany { outcome, seen }
}

/// Renders `config` (normalized when `normalize` is set) to `path`, recording
/// every reading and cancelling the token at the first one `cancel_at`
/// accepts.
fn run(
    config: &ExportConfig,
    normalize: Option<Normalize>,
    path: &std::path::Path,
    cancel_at: impl Fn(&Progress) -> bool,
) -> Run {
    let RunMany { outcome, seen } = run_many(config, normalize, &[path], cancel_at);
    Run {
        outcome: outcome.map(|mut produced| produced.remove(0).file().expect("a file output")),
        seen,
    }
}

/// The frames a reading has done, 0 for one that counts none.
fn done(p: &Progress) -> usize {
    p.frames().map_or(0, |(done, _)| done.get())
}

/// A render cancelled by its callback after N frames is `Cancelled` (not an
/// error, not a short file), has rendered exactly those frames, and leaves
/// nothing at its path, in every format.
///
/// Mutation (run): `open_file` writing to `path` itself (`Encoder::create` over
/// `File::create(path)`) -> the partial file stays and the `exists` assertion
/// fails.
/// Mutation (run): `drive` ignoring the token -> the outcome assertion fails.
#[test]
fn a_cancelled_file_render_is_cancelled_and_leaves_no_file() {
    let dir = tempfile::tempdir().unwrap();
    for (format, ext) in formats() {
        let path = dir.path().join(format!("a.{ext}"));
        let r = run(&config(format), None, &path, |p| {
            p.stage() == Stage::Rendering && done(p) >= 2 * 4096
        });
        assert_eq!(r.outcome, Outcome::Cancelled, "{ext}");
        assert_eq!(
            done(r.seen.last().unwrap()),
            2 * 4096,
            "{ext}: stopped at the reading that cancelled"
        );
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

/// A cancel never touches a file already at the path, whichever stage it
/// lands in, and leaves no temporary beside it: the render is written aside
/// and renamed over the target only when it finishes.
///
/// Mutation (run): `open_file` writing to `path` itself (no `Staged`) ->
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
        for (normalize, stage) in [
            (None, Stage::Rendering),
            (Some(Normalize::peak(Db(-6.0))), Stage::Rendering),
            (Some(Normalize::peak(Db(-6.0))), Stage::Writing),
        ] {
            std::fs::write(&path, &old).unwrap();
            let r = run(&config(format), normalize, &path, |p| {
                p.stage() == stage && done(p) >= 4096
            });
            assert_eq!(r.outcome, Outcome::Cancelled, "{ext} {stage:?}");
            assert_eq!(std::fs::read(&path).unwrap(), old, "{ext} {stage:?}");
            assert_eq!(
                names(dir.path()),
                std::slice::from_ref(&name),
                "{ext} {stage:?}"
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
        let written = render_to_file(graph(), &config(format), &FrozenClock, &path).unwrap();
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
        let err = render_to_file(graph(), &config(format), &FrozenClock, &target)
            .expect_err("a directory cannot be replaced by a file");
        assert!(matches!(err, tutti_export::Error::Io(_)), "{ext}: {err}");
        assert!(target.join("keep").exists(), "{ext}");
        assert_eq!(names(dir.path()), [format!("d.{ext}")], "{ext}");

        let missing = dir.path().join("no-such-dir").join(format!("m.{ext}"));
        let err = render_to_file(graph(), &config(format), &FrozenClock, &missing)
            .expect_err("the directory does not exist");
        assert!(matches!(err, tutti_export::Error::Io(_)), "{ext}: {err}");
    }
}

/// Rendering to memory stops the same way, with nothing returned, whether the
/// token is cancelled while it renders or as its last stage begins.
///
/// Mutation (run): `finalize_files` skipping the check after `begin` of the
/// finalizing stage -> the second case is `Finished`; fails.
#[test]
fn a_cancelled_buffer_render_is_cancelled() {
    let cfg = config(Format::Wav(Pcm::Int16));
    for at in [Stage::Rendering, Stage::Finalizing] {
        let token = CancelToken::new();
        let mut seen = Vec::new();
        let mut report = |p: &Progress| {
            seen.push(*p);
            if p.stage() == at && (at != Stage::Rendering || done(p) >= 4096) {
                token.cancel();
            }
        };
        let outcome = render_buffers_under(
            graph(),
            &cfg,
            &FrozenClock,
            &mut Control::new(&token).on_progress(&mut report),
        )
        .unwrap();
        assert!(matches!(outcome, Outcome::Cancelled), "{at:?}");
        assert_eq!(seen.last().unwrap().stage(), at, "{at:?}: nothing after");
    }
}

/// A normalized export renders, measures, writes and finalizes in order, and
/// cancelling in the writing stage cancels the export and removes the file,
/// after the whole render has run.
///
/// Mutation (run): the writing stage given a `Control::none()` (weighed as
/// normalized) instead of the host's -> the outcome is `Finished` and the
/// file exists; fails.
#[test]
fn the_writing_stage_of_a_normalized_export_stops() {
    let dir = tempfile::tempdir().unwrap();
    for (format, ext) in formats() {
        let path = dir.path().join(format!("n.{ext}"));
        let r = run(
            &config(format),
            Some(Normalize::peak(Db(-6.0))),
            &path,
            |p| p.stage() == Stage::Writing && done(p) >= 4096,
        );
        assert_eq!(r.outcome, Outcome::Cancelled, "{ext}");
        let total = Samples(BLOCKS * GRAPH_MAX_BLOCK.get());
        let rendering_end = r
            .seen
            .iter()
            .rfind(|p| p.stage() == Stage::Rendering)
            .unwrap();
        assert_eq!(
            rendering_end.frames(),
            Some((total, total)),
            "{ext}: the render ran to its end"
        );
        assert!(!path.exists(), "{ext}");
    }
}

/// Cancelling in the rendering stage of a normalized export never measures or
/// writes.
///
/// Mutation (run): `drive` without either of its token checks -> the render
/// runs on, and measuring readings follow; fails.
#[test]
fn cancelling_the_rendering_stage_of_a_normalized_export_never_measures() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("n.wav");
    let r = run(
        &config(Format::Wav(Pcm::Int16)),
        Some(Normalize::peak(Db(-6.0))),
        &path,
        |p| p.stage() == Stage::Rendering && done(p) >= 4096,
    );
    assert_eq!(r.outcome, Outcome::Cancelled);
    assert!(r.seen.iter().all(|p| p.stage() == Stage::Rendering));
    assert!(!path.exists());
}

/// A cancel on the final block's reading is `Cancelled` (the reading is
/// delivered before the render is known to be complete), and the file is not
/// delivered.
///
/// Mutation (run): `drive` not checking the token after it delivers progress
/// -> the next stage begins before the cancel is seen; fails.
#[test]
fn a_cancel_on_the_final_reading_is_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("last.wav");
    let r = run(&config(Format::Wav(Pcm::Int16)), None, &path, |p| {
        p.stage() == Stage::Rendering && p.frames().is_some_and(|(d, t)| d == t)
    });
    assert_eq!(r.outcome, Outcome::Cancelled);
    assert_eq!(
        r.seen.last().unwrap().stage(),
        Stage::Rendering,
        "the render went on to its next stage"
    );
    assert!(!path.exists());
}

/// The render's shape: how it is configured, for the matrices below.
#[derive(Clone, Copy, Debug)]
struct Shape {
    normalize: bool,
    resample: bool,
    /// Two outputs from the one pass: the second is FLAC at 16 bits and, with
    /// normalize, takes the first's gain.
    two: bool,
}

/// The shapes of a render of one output.
const SHAPES: [Shape; 4] = [
    Shape {
        normalize: false,
        resample: false,
        two: false,
    },
    Shape {
        normalize: false,
        resample: true,
        two: false,
    },
    Shape {
        normalize: true,
        resample: false,
        two: false,
    },
    Shape {
        normalize: true,
        resample: true,
        two: false,
    },
];

/// The shapes of a render of two outputs.
const TWO_OUTPUT_SHAPES: [Shape; 3] = [
    Shape {
        normalize: false,
        resample: false,
        two: true,
    },
    Shape {
        normalize: true,
        resample: false,
        two: true,
    },
    Shape {
        normalize: true,
        resample: true,
        two: true,
    },
];

impl Shape {
    fn config(self, frames: usize) -> ExportConfig {
        let mut cfg = config_of(Format::Wav(Pcm::Int16), frames);
        if self.resample {
            cfg.resample = Some(Resample::to(FILE_RATE));
        }
        cfg
    }

    fn normalize(self) -> Option<Normalize> {
        self.normalize.then(|| Normalize::peak(Db(-6.0)))
    }
}

/// One place in the pipeline at which doc 019 section 6.2 has a render check
/// its token, as the reading that cancels there.
struct Point {
    name: &'static str,
    /// Whether a render of this shape has the point.
    exists: fn(Shape) -> bool,
    /// The reading at which the callback cancels: the first one accepted.
    at: fn(Shape, &Progress) -> bool,
}

const POINTS: [Point; 12] = [
    Point {
        name: "before the first block",
        exists: |_| true,
        at: |_, p| p.stage() == Stage::Rendering && done(p) == 0,
    },
    Point {
        name: "mid render (every block)",
        exists: |_| true,
        at: |_, p| p.stage() == Stage::Rendering && done(p) >= 4096,
    },
    Point {
        name: "after the last block",
        exists: |_| true,
        at: |_, p| p.stage() == Stage::Rendering && p.frames().is_some_and(|(d, t)| d == t),
    },
    Point {
        name: "between render and measure",
        exists: |s| s.normalize,
        at: |_, p| p.stage() == Stage::Measuring && done(p) == 0,
    },
    Point {
        name: "between measure chunks",
        exists: |s| s.normalize,
        at: |_, p| p.stage() == Stage::Measuring && done(p) >= 48_000 && done(p) < LONG_FRAMES,
    },
    Point {
        name: "after the last measure chunk",
        exists: |s| s.normalize,
        at: |_, p| p.stage() == Stage::Measuring && p.frames().is_some_and(|(d, t)| d == t),
    },
    Point {
        name: "between measure and write",
        exists: |s| s.normalize,
        at: |_, p| p.stage() == Stage::Writing && done(p) == 0,
    },
    Point {
        name: "mid write pass",
        exists: |s| s.normalize,
        at: |_, p| p.stage() == Stage::Writing && done(p) >= 4096,
    },
    Point {
        name: "before the resample flush and the finalize",
        exists: |_| true,
        at: |_, p| p.stage() == Stage::Finalizing && p.fraction() < 0.96,
    },
    Point {
        name: "between outputs' finalize",
        exists: |s| s.two,
        at: |_, p| {
            p.stage() == Stage::Finalizing
                && p.output() == Some(0)
                && p.fraction() > 0.96
                && p.fraction() < 1.0
        },
    },
    Point {
        name: "before the commit",
        exists: |_| true,
        at: |_, p| p.stage() == Stage::Finalizing && p.fraction() > 0.97 && p.fraction() < 1.0,
    },
    Point {
        name: "(a reading after the commit cannot stop it)",
        exists: |_| true,
        at: |_, p| p.stage() == Stage::Finalizing && p.fraction() == 1.0,
    },
];

/// A cancel at each point of doc 019 section 6.2 that the pipeline owns, in
/// each shape of render (with and without normalize, with and without a
/// conversion, one output or two from the one pass): the render is
/// `Cancelled`, the next thing it would have done is not done (the cancelling
/// reading is the last one), no temporary is left, and every target is as it
/// was: absent where it did not exist, byte for byte where it did.
///
/// The last row is the exception the doc names: once the files are in place
/// the render has finished, and the cancel comes too late.
///
/// The two-output shapes carry the split step 2 deferred: every output's
/// encoder is finalized into its temporary first, then the token is checked,
/// then the renames. "between outputs' finalize" is the check before the
/// second output's finalize (the first is closed, beside its path, and is
/// removed with the rest), and "before the commit" is the one after the last
/// finalize, before the first rename.
///
/// Not pinned by a row, because the next check catches the same cancel
/// before anything observable happens: the check after `begin(Measuring)`
/// (the chunk loop's own top-of-loop check comes next) and the one at the top
/// of `drive`'s loop (a cancel at its entry reading is also caught after the
/// first block; `driver::tests` pins that one).
#[test]
fn a_cancel_at_each_pipeline_point_stops_there_and_leaves_the_targets() {
    let old = b"an earlier export, byte for byte".to_vec();
    for shape in SHAPES.into_iter().chain(TWO_OUTPUT_SHAPES) {
        for point in &POINTS {
            if !(point.exists)(shape) {
                continue;
            }
            for existing in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let paths: Vec<std::path::PathBuf> = if shape.two {
                    vec![dir.path().join("t.wav"), dir.path().join("u.flac")]
                } else {
                    vec![dir.path().join("t.wav")]
                };
                if existing {
                    for path in &paths {
                        std::fs::write(path, &old).unwrap();
                    }
                }
                let before = names(dir.path());
                let refs: Vec<&std::path::Path> = paths.iter().map(|p| p.as_path()).collect();
                let r = run_many(&shape.config(LONG_FRAMES), shape.normalize(), &refs, |p| {
                    (point.at)(shape, p)
                });
                let ctx = format!("{shape:?} at `{}` (existing {existing})", point.name);
                let at = |p: &Progress| (point.at)(shape, p);
                if point.name.starts_with('(') {
                    // Too late: finished, and the new files are in place.
                    assert!(matches!(r.outcome, Outcome::Finished(_)), "{ctx}");
                    for path in &paths {
                        assert_ne!(std::fs::read(path).ok().as_deref(), Some(&old[..]), "{ctx}");
                    }
                    continue;
                }
                assert_eq!(r.outcome.finished().map(|_| ()), None, "{ctx}: finished");
                let hits = r.seen.iter().filter(|p| at(p)).count();
                assert_eq!(
                    (hits, at(r.seen.last().unwrap())),
                    (1, true),
                    "{ctx}: it went on after the cancelling reading"
                );
                assert_eq!(names(dir.path()), before, "{ctx}: a temporary or a target");
                for path in &paths {
                    if existing {
                        assert_eq!(std::fs::read(path).unwrap(), old, "{ctx}");
                    } else {
                        assert!(!path.exists(), "{ctx}");
                    }
                }
            }
        }
    }
}

/// Every reading of a render, in every shape: the fraction never falls; the
/// first reading is the first stage the pipeline owns, at the start of its
/// band; the last is `Finalizing` at exactly 1.0, delivered after the file is
/// in place and not before; each stage's readings stay inside its band of
/// doc 019 section 6.1; `output` is `None` while rendering and `Some(0)`
/// after; and no reading is `1.0` but the last.
///
/// Mutations (each run, each failed): `NORMALIZED`'s `Measuring` band
/// widened to 0.70 (the band check); `finalize_files` delivering
/// `at(Finalizing, 1.0)` before the renames (the one-reading-at-1.0 check);
/// `deliver` giving `Rendering` an output (the output check). Monotonic is
/// the unit test's (`the_fraction_never_falls`), which reorders stages.
#[test]
fn progress_is_monotonic_banded_and_ends_on_one_after_the_rename() {
    for shape in SHAPES {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.wav");
        let token = CancelToken::new();
        let mut seen: Vec<(Progress, bool)> = Vec::new();
        let mut report = |p: &Progress| seen.push((*p, path.exists()));
        let mut control = Control::new(&token).on_progress(&mut report);
        let cfg = shape.config(BLOCKS * GRAPH_MAX_BLOCK.get());
        let outcome = render_file_under(
            graph(),
            &cfg,
            &FrozenClock,
            shape.normalize(),
            &path,
            &mut control,
        )
        .unwrap();
        assert!(matches!(outcome, Outcome::Finished(_)), "{shape:?}");

        let band = |stage: Stage| -> (f32, f32) {
            match (shape.normalize, stage) {
                (false, Stage::Rendering) => (0.05, 0.95),
                (true, Stage::Rendering) => (0.03, 0.55),
                (true, Stage::Measuring) => (0.55, 0.62),
                (true, Stage::Writing) => (0.62, 0.95),
                (_, Stage::Finalizing) => (0.95, 1.0),
                (_, other) => panic!("{shape:?} has no {other:?} stage"),
            }
        };
        let readings: Vec<Progress> = seen.iter().map(|(p, _)| *p).collect();
        assert!(
            readings
                .windows(2)
                .all(|w| w[0].fraction() <= w[1].fraction()),
            "{shape:?}: not monotonic"
        );
        let first = readings.first().unwrap();
        assert_eq!(first.stage(), Stage::Rendering, "{shape:?}");
        assert_eq!(first.fraction(), band(Stage::Rendering).0, "{shape:?}");
        let last = readings.last().unwrap();
        assert_eq!((last.stage(), last.fraction()), (Stage::Finalizing, 1.0));
        assert_eq!(
            readings.iter().filter(|p| p.fraction() >= 1.0).count(),
            1,
            "{shape:?}: only the last reading is 1.0"
        );
        for (p, exists) in &seen {
            let (from, to) = band(p.stage());
            assert!(
                (from..=to).contains(&p.fraction()),
                "{shape:?}: {:?} at {} is outside {from}..{to}",
                p.stage(),
                p.fraction()
            );
            assert_eq!(
                p.output(),
                (p.stage() != Stage::Rendering).then_some(0),
                "{shape:?} {:?}",
                p.stage()
            );
            assert_eq!(
                *exists,
                p.fraction() == 1.0,
                "{shape:?}: the file is in place exactly at the 1.0 reading"
            );
        }
        // Stages come in order, and each is entered at the start of its band.
        let stages: Vec<Stage> = {
            let mut v: Vec<Stage> = readings.iter().map(Progress::stage).collect();
            v.dedup();
            v
        };
        let want: &[Stage] = if shape.normalize {
            &[
                Stage::Rendering,
                Stage::Measuring,
                Stage::Writing,
                Stage::Finalizing,
            ]
        } else {
            &[Stage::Rendering, Stage::Finalizing]
        };
        assert_eq!(stages, want, "{shape:?}");
        for stage in want {
            let entry = readings.iter().find(|p| p.stage() == *stage).unwrap();
            assert_eq!(entry.fraction(), band(*stage).0, "{shape:?} {stage:?}");
        }
    }
}

/// On a long render no two readings of a stage are more than 4096 frames
/// apart, a stage change is always delivered, and the render is not reported
/// every block.
///
/// Mutations (each run, each failed): `DELIVERY_FRAMES` 4096 -> 8192 -> the
/// gap check fails; `advance` delivering only on completion -> the gap check
/// fails (and on every call, the `readings < blocks` check).
#[test]
fn readings_come_at_least_every_4096_frames_and_at_every_stage_change() {
    let blocks = 200;
    let frames = blocks * GRAPH_MAX_BLOCK.get();
    for shape in [SHAPES[0], SHAPES[2]] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.wav");
        let r = run(&shape.config(frames), shape.normalize(), &path, |_| false);
        assert!(matches!(r.outcome, Outcome::Finished(_)));
        for stage in [Stage::Rendering, Stage::Writing] {
            let at: Vec<usize> = r
                .seen
                .iter()
                .filter(|p| p.stage() == stage)
                .map(done)
                .collect();
            if at.is_empty() {
                continue;
            }
            assert_eq!(at.first(), Some(&0), "{shape:?} {stage:?} entry");
            assert_eq!(at.last(), Some(&frames), "{shape:?} {stage:?} completion");
            assert!(
                at.windows(2).all(|w| w[1] - w[0] <= 4096),
                "{shape:?} {stage:?}: a gap past 4096 frames"
            );
            assert!(at.len() < blocks, "{shape:?} {stage:?}: one per block");
        }
        let stages: Vec<Stage> = {
            let mut v: Vec<Stage> = r.seen.iter().map(Progress::stage).collect();
            v.dedup();
            v
        };
        assert_eq!(
            stages.len(),
            if shape.normalize { 4 } else { 2 },
            "{shape:?}: every stage change was delivered"
        );
    }
}

/// With two outputs a reading names the output it is working on: none while
/// rendering (one render serves both), the measured one while measuring (only
/// the first measures itself; the second takes its gain), each in turn while
/// writing, and each as it is finalized.
#[test]
fn a_reading_names_the_output_it_is_working_on() {
    let shape = TWO_OUTPUT_SHAPES[1];
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a.wav"), dir.path().join("b.flac"));
    let r = run_many(
        &shape.config(LONG_FRAMES),
        shape.normalize(),
        &[&a, &b],
        |_| false,
    );
    assert!(matches!(r.outcome, Outcome::Finished(_)));
    let outputs_of = |stage: Stage| -> Vec<Option<usize>> {
        let mut v: Vec<Option<usize>> = r
            .seen
            .iter()
            .filter(|p| p.stage() == stage)
            .map(Progress::output)
            .collect();
        v.dedup();
        v
    };
    assert_eq!(outputs_of(Stage::Rendering), [None]);
    assert_eq!(outputs_of(Stage::Measuring), [Some(0)]);
    assert_eq!(outputs_of(Stage::Writing), [Some(0), Some(1)]);
    assert_eq!(outputs_of(Stage::Finalizing), [Some(0), Some(1)]);
    let writing: Vec<usize> = r
        .seen
        .iter()
        .filter(|p| p.stage() == Stage::Writing)
        .map(done)
        .collect();
    assert_eq!(
        (writing.first(), writing.last()),
        (Some(&0), Some(&(2 * LONG_FRAMES))),
        "writing counts the frames of both outputs against their sum"
    );
}
