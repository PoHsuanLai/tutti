use super::*;
use tutti_graph::{Cx, Editor, Executor, Io, Node, Prepare, Shape, Status, Transport, Unforkable};
use tutti_types::graph::{OutPort, Source};
use tutti_types::{Beat, Bpm, NodeKey, SampleRate, TapAlign, TapId};

/// A stereo source that plays `frames` in order, then silence: the graph the
/// tap copies out of.
struct Play {
    frames: Vec<(f32, f32)>,
    at: usize,
}

impl Node for Play {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::STEREO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let n = io.output(0).len();
        for k in 0..n {
            let (l, r) = self.frames.get(self.at + k).copied().unwrap_or_default();
            io.output(0)[k] = l;
            io.output(1)[k] = r;
        }
        self.at += n;
        Status::Modified
    }
    fn reset(&mut self) {}
}

const BLOCK: usize = 512;

/// The tap's reader over a graph that has rendered `frames`, through the real
/// tap op the audio thread runs: the executor copies the source's two
/// channels into the tap's lanes, and the reader interleaves them.
fn tap_with(frames: &[(f32, f32)]) -> TapIn {
    tap_with_ring(frames, frames.len().max(1))
}

/// [`tap_with`], the rings holding `ring` frames.
fn tap_with_ring(frames: &[(f32, f32)], ring: usize) -> TapIn {
    let (mut ed, mut exec): (Editor, Executor) =
        Editor::new(Prepare::new(SampleRate(48_000.0), Samples(BLOCK)));
    ed.insert(
        NodeKey(1),
        "play",
        Unforkable(Play {
            frames: frames.to_vec(),
            at: 0,
        }),
    );
    let port = |port| {
        Source::Node(OutPort {
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
            Samples(ring),
        )
        .expect("the sources exist");
    ed.commit().expect("compiles");
    exec.apply_pending();
    ed.collect();
    let t = Transport::new(true, Bpm::of(120.0), Beat(0.0), None);
    let mut left = [0.0f32; BLOCK];
    let mut right = [0.0f32; BLOCK];
    for block in frames.chunks(BLOCK) {
        let mut outs: [&mut [f32]; 2] = [&mut left[..block.len()], &mut right[..block.len()]];
        exec.process(block.len(), &t, &[], &mut outs);
    }
    TapIn::new(reader)
}

/// Frames the graph rendered come back out in order, interleaved L then R.
///
/// The tap keeps one ring a lane and `AudioIn` speaks a flat interleaved
/// slice; this is the conversion, so a transposition here would silently
/// swap every recording's channels.
///
/// Mutation (run, recorded here): swap the two lanes in `TapReader::pop_into`
/// (`skip(width - 1 - channel)`) -> the channels come back swapped and the
/// first assertion fails.
#[test]
fn rendered_frames_come_back_in_order_and_channel_side() {
    let frames: Vec<(f32, f32)> = (0..4).map(|i| (i as f32, -(i as f32))).collect();
    let mut tap = tap_with(&frames);
    let mut out = [0.0f32; 8 * 2];
    assert_eq!(
        tap.poll_into(&mut out),
        Samples(4),
        "the return is FRAMES, not samples"
    );

    for i in 0..4 {
        assert_eq!(out[i * 2], i as f32, "left channel, frame {i}");
        assert_eq!(out[i * 2 + 1], -(i as f32), "right channel, frame {i}");
    }
}

/// A poll is bounded by the output slice, not by what the ring holds.
///
/// The pump hands a fixed scratch buffer every pass; writing past it would
/// be the kind of overrun no test above would notice.
#[test]
fn a_poll_never_writes_past_the_output_slice() {
    let mut tap = tap_with(&[(1.0, 1.0); 32]);
    let mut out = [0.0f32; 4 * 2];
    assert_eq!(
        tap.poll_into(&mut out),
        Samples(4),
        "must fill exactly the slice"
    );

    // The rest is still queued, not dropped.
    let mut rest = [0.0f32; 32 * 2];
    assert_eq!(tap.poll_into(&mut rest), Samples(28));
}

/// An empty tap yields nothing and says so — without claiming the end.
///
/// The `ON_EMPTY` assertion is the load-bearing half: a `Starved` source
/// makes a pump park and retry, and the recording survives the gap between
/// audio callbacks. `EndOfStream` here would end every take immediately.
#[test]
fn an_empty_tap_starves_rather_than_ending() {
    let mut tap = tap_with(&[]);

    let mut out = [0.0f32; 4 * 2];
    assert_eq!(tap.poll_into(&mut out), Samples::ZERO);
    assert_eq!(
        TapIn::ON_EMPTY,
        OnEmpty::Starved,
        "the graph does not end; an empty tap is a gap, not a finish"
    );
}

/// The join this type exists for: a tap consumer satisfies the bound
/// `Recorder::start` and `pump` require.
///
/// Asserted as a compile-time bound rather than by running a recorder — the
/// defect it guards against is `TapIn` failing to *be* an `AudioIn`, which
/// is a type error, not a behaviour.
#[test]
fn a_tap_is_accepted_where_a_live_source_is_required() {
    fn takes_a_live_source<I: AudioIn + Send + 'static>(_: I) {}

    takes_a_live_source(tap_with(&[]));
}

/// Its layout is the tap's width, not a fixed stereo: a six-lane tap is a
/// six-channel source.
///
/// Mutation (run, recorded here): `TapIn::layout` returning
/// `ChannelLayout::STEREO` -> a one-lane and a six-lane tap read as stereo
/// and this fails.
#[test]
fn the_layout_follows_the_taps_width() {
    let (mut ed, _exec): (Editor, Executor) =
        Editor::new(Prepare::new(SampleRate(48_000.0), Samples(BLOCK)));
    for width in [1usize, 2, 6] {
        let reader = ed
            .open_tap(
                TapId(width as u32),
                vec![Source::Zero; width],
                TapAlign::Raw,
                Samples(8),
            )
            .expect("the sources exist");
        assert_eq!(
            TapIn::new(reader).layout(),
            ChannelLayout::from_count(width as u16)
        );
    }
}

/// The frames a tap's rings refused are the source's `take_lost`, once.
///
/// Mutation (run, recorded here): `TapIn::take_lost` returning the trait's
/// zero -> this fails.
#[test]
fn a_tap_reports_the_frames_it_lost() {
    let mut tap = tap_with_ring(&[(1.0, 1.0); 8], 5);
    assert_eq!(tap.take_lost(), Samples(3));
    assert_eq!(tap.take_lost(), Samples::ZERO, "reading clears");
}
