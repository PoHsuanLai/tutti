use super::*;
use crate::spill::SPILL_MEMORY_BYTES;
use crate::{BufferTarget, FrozenClock, Normalize, NormalizeSpec, Pcm, Progress};
use tutti_graph::GraphBuilder;
use tutti_types::Samples;

const RATE: SampleRate = SampleRate(48_000.0);
const FRAMES: usize = 24_000;

/// Stereo constants, a different one per channel.
fn graph() -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let id = g.add(tutti_nodes::testing::Const::frame(&[0.25, 0.125]));
    g.pipe_output(id);
    let (editor, executor) = g.build(RenderGraph::prepare(RATE)).expect("builds");
    RenderGraph::new(editor, executor).expect("built together")
}

fn names(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// A float WAV normalized to a peak, and the planes of the same pass beside it,
/// rendered with `budget` bytes of memory for the spill. Returns the file
/// written, the planes, and whether a spill file was seen beside the target
/// at any reading of the render.
fn run(budget: usize, dir: &std::path::Path) -> (crate::Written, Vec<Vec<f32>>, bool) {
    let path = dir.join("n.wav");
    let job = Job::new(RATE, Samples(FRAMES), Samples(0), Samples(0)).unwrap();
    let mut seen_spill = false;
    let mut watch = |_: &Progress| {
        seen_spill |= names(dir).iter().any(|n| n.ends_with(".spill"));
    };
    let token = crate::CancelToken::new();
    let mut control = Control::new(&token).on_progress(&mut watch);
    let target = crate::FileTarget::new(
        &path,
        crate::Format::Wav(Pcm::Float32),
        ChannelLayout::STEREO,
    )
    .with_normalize(Some(NormalizeSpec::own(Normalize::peak(Db(-6.0)))));
    let produced = render_outputs(
        &mut graph(),
        &job,
        &FrozenClock,
        vec![
            Output::master(target),
            Output::master(BufferTarget::new(ChannelLayout::STEREO)),
        ],
        &mut control,
        budget,
    )
    .expect("renders")
    .finished()
    .expect("not cancelled");
    let mut produced = produced.into_iter();
    let written = produced.next().unwrap().file().unwrap();
    let planes = produced.next().unwrap().buffers().unwrap().into_planes();
    (written, planes, seen_spill)
}

/// A normalizing output keeps its audio in memory within the budget and in a
/// temporary file beside the target past it, and the file it writes is the same
/// either way: each sample is the plane the same pass held in memory, times the
/// gain `Written` reports, which is the peak target less the true peak that was
/// measured. Not a comparison of two renders: each run is held to the planes
/// and the gain of its own pass.
///
/// This is the seam for the threshold: `render_outputs` takes the memory
/// budget as an argument (the public `render` passes
/// `SPILL_MEMORY_BYTES`, 512 MiB), so a test spills with a budget of zero
/// rather than rendering that much.
#[test]
fn a_normalized_output_spills_past_the_budget_and_writes_what_it_would_in_memory() {
    for (budget, spills) in [(0, true), (SPILL_MEMORY_BYTES, false)] {
        let dir = tempfile::tempdir().unwrap();
        let (written, planes, seen_spill) = run(budget, dir.path());
        assert_eq!(seen_spill, spills, "budget {budget}");
        assert_eq!(
            names(dir.path()),
            ["n.wav"],
            "budget {budget}: the spill file was removed"
        );

        let gain = written.gain.expect("normalized");
        let reading = written.measured.expect("measured");
        assert!(
            (gain.get() - (-6.0 - reading.true_peak.get())).abs() < 1e-5,
            "budget {budget}: {gain:?} for {reading:?}"
        );
        let scale = gain.to_amplitude().get();
        let mut file = hound::WavReader::open(dir.path().join("n.wav")).unwrap();
        assert_eq!(file.spec().sample_format, hound::SampleFormat::Float);
        let samples: Vec<f32> = file.samples::<f32>().map(|s| s.unwrap()).collect();
        assert_eq!(samples.len(), FRAMES * 2, "budget {budget}");
        for (i, frame) in samples.chunks(2).enumerate() {
            for c in 0..2 {
                assert_eq!(
                    frame[c],
                    planes[c][i] * scale,
                    "budget {budget}: frame {i} channel {c}"
                );
            }
        }
    }
}

/// A render that is cancelled while it writes, or that fails after it spilled
/// (here: silence, which has no loudness to take a gain from), leaves neither
/// a spill file nor a temporary nor the target behind.
#[test]
fn a_cancelled_or_failed_render_removes_its_spill() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("n.wav");
    let job = Job::new(RATE, Samples(FRAMES), Samples(0), Samples(0)).unwrap();
    let target = || {
        Output::master(
            crate::FileTarget::new(
                &path,
                crate::Format::Wav(Pcm::Float32),
                ChannelLayout::STEREO,
            )
            .with_normalize(Some(NormalizeSpec::own(Normalize::peak(Db(-6.0))))),
        )
    };

    let token = crate::CancelToken::new();
    let mut spilled = false;
    let mut cancel_in_writing = |p: &Progress| {
        spilled |= names(dir.path()).iter().any(|n| n.ends_with(".spill"));
        if p.stage() == Stage::Writing {
            token.cancel();
        }
    };
    let outcome = render_outputs(
        &mut graph(),
        &job,
        &FrozenClock,
        vec![target()],
        &mut Control::new(&token).on_progress(&mut cancel_in_writing),
        0,
    )
    .expect("a cancel is not an error");
    assert!(outcome.finished().is_none(), "cancelled");
    assert!(spilled, "it had spilled when it was cancelled");
    assert!(names(dir.path()).is_empty());

    let mut silent = {
        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
        let id = g.add(tutti_nodes::testing::Const::frame(&[0.0, 0.0]));
        g.pipe_output(id);
        let (editor, executor) = g.build(RenderGraph::prepare(RATE)).expect("builds");
        RenderGraph::new(editor, executor).expect("built together")
    };
    let err = render_outputs(
        &mut silent,
        &job,
        &FrozenClock,
        vec![target()],
        &mut Control::none(),
        0,
    )
    .expect_err("silence has no reading");
    assert!(matches!(err, Error::Unmeasurable { .. }), "{err:?}");
    assert!(names(dir.path()).is_empty());
}
