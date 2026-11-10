//! The end-to-end this crate describes but never ran: **graph tap →
//! `PumpLoop` → `FileOut` → read it back**.
//!
//! Every piece had unit coverage and the whole had none. `tap_in.rs`'s tests
//! push frames and pop them again; `wav_out.rs`'s write into a sink and read
//! the file; `recorder.rs`'s pump a fixture. Nothing put the three together,
//! so nothing checked that the *units* line up across the seams — and the
//! units are the entire risk surface here. `CLAUDE.md` names the failure
//! exactly: "**Every count on this boundary is denominated in FRAMES** … a
//! consumer compares a returned count against a loop range or a file position,
//! both in frames, so leaking samples makes a 6-channel looped clip wrap at a
//! sixth of its length and present as 'the loop points are wrong'."
//!
//! Values, not just counts. Each frame carries its own index, so a dropped,
//! duplicated, or channel-swapped frame shows up as a wrong *number* rather
//! than a right total. A count-only assertion passes on all three.
//!
//! The tap is the real one: an executor renders a source whose frames carry
//! their own index, and the plan's tap op copies them into the rings the
//! reader drains.
//!
//! Mutations (run against `TapReader::pop_into` and `tap_op` in `tutti-graph`;
//! the frames→samples and left/right swap rows are also
//! `tutti-graph`'s `tests/tap_audio_in.rs`
//! `rendered_frames_come_back_in_order_and_channel_side`, and these):
//!
//! | mutation | fails |
//! |---|---|
//! | `.min(out.len() / width)` → `.min(out.len())` in `pop_into` (frames→samples) | `…_in_order_through_the_whole_chain` only (it panics past the pump's buffer) |
//! | swap the lanes (`skip(width - 1 - channel)`) | both |
//!
//! `FRAMES` stays above the pump's scratch so a push that *exceeds* it forces
//! a bad bound to be used.

use tutti_codec::{FileOut, Format, Pcm, Spec};
use tutti_graph::{
    Cx, Editor, Executor, Io, Node, Prepare, Shape, Status, TapReader, Transport, Unforkable,
};
use tutti_graph::{NodeKey, TapAlign, TapId};
use tutti_graph::{OutPort, Source as Origin};
use tutti_session::{PumpLoop, PumpPass};
use tutti_types::{Beat, Bpm, ChannelLayout, SampleRate, Samples, Sink, Source};

/// Frames rendered into the tap, as the audio callback would.
const FRAMES: usize = 4096;

/// A value that identifies its own frame index exactly under f32.
fn left_of(i: usize) -> f32 {
    i as f32 / 65_536.0
}

/// Distinct from the left channel at every index, so a channel swap is visible.
fn right_of(i: usize) -> f32 {
    -(i as f32 / 65_536.0) - 0.5
}

/// A stereo source whose frame `i` is `(left_of(i), right_of(i))`, counted
/// across blocks.
struct Ramp {
    at: usize,
}

impl Node for Ramp {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::STEREO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for k in 0..io.output(0).len() {
            io.output(0)[k] = left_of(self.at + k);
            io.output(1)[k] = right_of(self.at + k);
        }
        self.at += io.output(0).len();
        Status::Modified
    }
    fn reset(&mut self) {}
}

const BLOCK: usize = 512;

/// The graph the tap copies out of: a [`Ramp`] on a master tap.
struct Graph {
    exec: Executor,
    _ed: Editor,
}

impl Graph {
    /// Renders `n` frames, in blocks.
    fn render(&mut self, n: usize) {
        let t = Transport::new(true, Bpm::of(120.0), Beat(0.0), None);
        let (mut left, mut right) = ([0.0f32; BLOCK], [0.0f32; BLOCK]);
        let mut left_over = n;
        while left_over > 0 {
            let len = left_over.min(BLOCK);
            let mut outs: [&mut [f32]; 2] = [&mut left[..len], &mut right[..len]];
            self.exec.process(len, &t, &[], &mut outs);
            left_over -= len;
        }
    }
}

/// Runs passes until one writes nothing, or `budget` passes have run, and
/// returns the total frames written.
fn pump_until_dry<I: Source, O: Sink>(pump: &mut PumpLoop<I, O>, budget: usize) -> Samples {
    let mut total = Samples::ZERO;
    for _ in 0..budget {
        match pump.pump_once() {
            PumpPass::Wrote(n) => total += n,
            _ => break,
        }
    }
    total
}

/// A stereo tap on a [`Ramp`], its ring `capacity` frames.
fn graph_tap(capacity: usize) -> (Graph, TapReader) {
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(BLOCK)));
    ed.insert(NodeKey(1), "ramp", Unforkable(Ramp { at: 0 }));
    let port = |port| {
        Origin::Node(OutPort {
            node: NodeKey(1),
            port,
        })
    };
    ed.spec_mut().topology.outputs = vec![port(0), port(1)];
    let reader = ed
        .open_tap(
            TapId(0),
            vec![port(0), port(1)],
            TapAlign::ToOutput,
            Samples(capacity),
        )
        .expect("the sources exist");
    ed.commit().expect("compiles");
    exec.apply_pending();
    ed.collect();
    (Graph { exec, _ed: ed }, reader)
}

/// **The whole chain, driven pass by pass, with every sample checked.**
///
/// A stepped `PumpLoop` rather than a thread: the counts are then exact rather
/// than whatever the scheduler allowed, which is the same argument
/// `recorder.rs` makes for its own fixtures. The thread is covered separately
/// in `recorder_thread.rs`.
#[test]
fn tapped_frames_reach_the_wav_in_order_through_the_whole_chain() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tapped.wav");

    let (mut graph, reader) = graph_tap(FRAMES);
    let src = reader;
    assert_eq!(
        Source::layout(&src),
        ChannelLayout::STEREO,
        "the tap has two lanes; the sink below is built to match"
    );

    let sink = FileOut::create(
        &path,
        &Spec::new(
            Format::Wav(Pcm::Float32),
            ChannelLayout::STEREO,
            SampleRate(48_000.0),
        ),
    )
    .expect("sink opens");

    let mut pump =
        PumpLoop::new(src, sink, Samples(1024)).expect("stereo source into a stereo sink");

    // Fill the tap first so the pump has something to move, then drain it with
    // a bounded number of passes — never a sleep, and never "pump until it
    // looks done".
    graph.render(FRAMES);
    let moved = pump_until_dry(&mut pump, 64);
    assert_eq!(
        moved,
        Samples(FRAMES),
        "every frame pushed must be moved, and counted in FRAMES — a pump that \
         returned samples here would report {}",
        FRAMES * 2
    );
    // The tap is empty now, so the next pass must report a live source waiting
    // rather than a finished one. `TapReader::pull` answers `Starved` for an empty
    // tap, and this is what that answer buys. Mutation (run): answering `Ended`
    // there makes this pass report `Ended` and fails the assertion.
    assert_eq!(
        pump.pump_once(),
        PumpPass::Starved,
        "an empty tap is a producer that has not caught up, not end-of-stream"
    );

    pump.finalize().expect("the take finalizes cleanly");

    let mut reader = hound::WavReader::open(&path).expect("finalized WAV should be readable");
    assert_eq!(reader.spec().channels, 2);
    assert_eq!(reader.spec().sample_rate, 48_000);
    assert_eq!(
        reader.len() as usize,
        FRAMES * 2,
        "the header counts samples; the chain counts frames. {FRAMES} frames \
         must land as {} samples",
        FRAMES * 2
    );

    let samples: Vec<f32> = reader.samples::<f32>().map(|s| s.unwrap()).collect();
    for (i, pair) in samples.as_chunks::<2>().0.iter().enumerate() {
        assert!(
            (pair[0] - left_of(i)).abs() < 1e-9,
            "frame {i} left: expected {}, got {}",
            left_of(i),
            pair[0]
        );
        assert!(
            (pair[1] - right_of(i)).abs() < 1e-9,
            "frame {i} right: expected {}, got {} (a channel swap looks like this)",
            right_of(i),
            pair[1]
        );
    }
}

/// **A tap filled in several bursts still lands one contiguous take.**
///
/// The single-burst case above cannot distinguish "moves frames correctly"
/// from "moves exactly one block correctly". Pushing across several passes
/// exercises the ring wrapping and the pump's repeated entry, and a
/// frames-vs-samples slip shows as a *short* file rather than a reordered one.
#[test]
fn interleaved_pushes_and_pumps_lose_no_frames() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bursty.wav");

    let (mut graph, reader) = graph_tap(2 * 512);
    let src = reader;
    let sink = FileOut::create(
        &path,
        &Spec::new(
            Format::Wav(Pcm::Float32),
            ChannelLayout::STEREO,
            SampleRate(48_000.0),
        ),
    )
    .expect("sink opens");

    let mut pump =
        PumpLoop::new(src, sink, Samples(1024)).expect("stereo source into a stereo sink");

    const BURST: usize = 512;
    const BURSTS: usize = 6;
    let mut moved = Samples::ZERO;
    for _ in 0..BURSTS {
        graph.render(BURST);
        moved += pump_until_dry(&mut pump, 16);
    }
    assert_eq!(
        moved,
        Samples(BURST * BURSTS),
        "no frame may be lost between bursts"
    );

    pump.finalize().expect("the take finalizes cleanly");

    let mut reader = hound::WavReader::open(&path).expect("finalized WAV should be readable");
    assert_eq!(reader.len() as usize, BURST * BURSTS * 2);

    let samples: Vec<f32> = reader.samples::<f32>().map(|s| s.unwrap()).collect();
    for (i, pair) in samples.as_chunks::<2>().0.iter().enumerate() {
        assert!(
            (pair[0] - left_of(i)).abs() < 1e-9 && (pair[1] - right_of(i)).abs() < 1e-9,
            "frame {i} is not the frame that was pushed at that index — the \
             bursts did not concatenate"
        );
    }
}

/// **A recording through a tap that fell behind reports the gap, and the file
/// closes up over it.** The ring holds 512 frames and 2048 are rendered before
/// the pump runs, so the first 512 are kept and 1536 are lost. The recorder
/// counts them; the file holds the 512 and is shorter than the take by the
/// lost frames, with nothing in the audio to mark where.
///
/// Mutation (run, recorded here): delete `impl Source for TapReader`'s `take_lost` (the trait's
/// zero default) -> `lost()` reads 0 and the first assertion fails. Delete the
/// `fetch_add` in `PumpLoop::pump_once` -> the same.
#[test]
fn a_gap_in_a_take_is_counted_and_the_file_closes_up_over_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gappy.wav");
    let (mut graph, reader) = graph_tap(512);
    let src = reader;
    let sink = FileOut::create(
        &path,
        &Spec::new(
            Format::Wav(Pcm::Float32),
            ChannelLayout::STEREO,
            SampleRate(48_000.0),
        ),
    )
    .expect("sink opens");
    let mut pump = PumpLoop::new(src, sink, Samples(1024)).expect("pairs");

    graph.render(2048);
    let moved = pump_until_dry(&mut pump, 64);
    assert_eq!(moved, Samples(512));
    assert_eq!(pump.lost(), Samples(1536), "rendered = recorded + lost");
    pump.finalize().expect("finalizes");

    let reader = hound::WavReader::open(&path).expect("readable");
    assert_eq!(
        reader.len() as usize,
        512 * 2,
        "the file is the frames that arrived"
    );
}
