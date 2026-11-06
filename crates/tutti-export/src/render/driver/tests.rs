use super::*;
use tutti_core::transport::{FrozenClock, OfflineTimeline, OfflineTimelineConfig};
use tutti_core::{Beat, Bpm, SampleRate};
use tutti_graph::GraphBuilder;
use tutti_types::ChannelLayout;

const RATE: SampleRate = SampleRate(48_000.0);

/// A mono graph whose one output carries a constant.
fn mono_dc(v: f32) -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let k = g.add(tutti_nodes::testing::Const::mono(v));
    g.pipe_output(k);
    let (editor, executor) = g.build(RenderGraph::prepare(RATE)).expect("builds");
    RenderGraph::new(editor, executor).expect("built together")
}

/// Render `frames` frames of `graph` at a runtime width under `clock`,
/// returned as owned frames.
fn render(
    graph: &mut RenderGraph,
    clock: &dyn RenderClock,
    ch: usize,
    frames: usize,
) -> Vec<Vec<f32>> {
    let plan = crate::render::RenderPlan {
        total: Samples(frames),
        output_length: Samples(frames),
        latency: Samples(0),
    };
    let mut out = Vec::new();
    with_source(graph, RATE, clock, |src| {
        drive(src, ChannelLayout::from(ch), &plan, |b| {
            out.extend(b.iter_frames().map(<[f32]>::to_vec));
            Ok(())
        })
    })
    .unwrap();
    out
}

/// A mono graph rendered to a wider file must NOT be copied into every
/// channel. `fold_frame`'s rule beyond stereo is copy-and-zero-fill, and a
/// full-level mono copy in the LFE and surrounds is the bug this pins.
///
/// Mutation (run): `fold_graph_frame` writing `dst.fill(src[0])` for a
/// one-channel source → channel 1 reads 0.5.
#[test]
fn a_mono_graph_does_not_fill_every_surround_channel() {
    let frames = render(&mut mono_dc(0.5), &FrozenClock, 6, 64);
    assert!(!frames.is_empty());
    for f in &frames {
        assert_eq!(f.len(), 6);
        assert_eq!(f[0], 0.5, "channel 0 carries the mono signal");
        for (c, &s) in f.iter().enumerate().skip(1) {
            assert_eq!(s, 0.0, "channel {c} must be silent, got {s}");
        }
    }
}

/// Rendering a graph NARROWER than the file must not panic — a gather
/// sized to the frame width rather than to the graph's outputs indexes
/// past its planes on every upmix.
///
/// Widths 3 and 5 are here on purpose: they are the ones a fixed enumeration
/// of widths would refuse outright.
///
/// Mutation (run): `fold_graph_frame` gathering `dst.len()` channels
/// instead of `planes.len()` → it indexes a missing plane and panics.
#[test]
fn rendering_a_narrow_graph_to_a_wide_file_does_not_panic() {
    for ch in [2usize, 3, 4, 5, 12] {
        let frames = render(&mut mono_dc(0.25), &FrozenClock, ch, 32);
        assert_eq!(frames.len(), 32, "width {ch}");
        assert!(frames.iter().all(|f| f.len() == ch), "width {ch}");
    }
}

/// `GraphSource` must NOT prime the clock: the render is
/// emit-then-advance, so the first rendered frame carries the start
/// beat, and after N frames the clock stands exactly N frames on. A
/// priming `advance` desyncs every clock reader from the render's start.
///
/// The graph reads the beat from each block's transport, which this
/// source hands it; here a test node calls `Env::for_each_beat`.
///
/// Mutation (run): `self.clock.advance(Samples(1))` before `render_graph`
/// in `GraphSource::fill` → the first frame reads a frame past the start
/// beat.
#[test]
fn the_clock_advances_exactly_once_per_frame_and_never_ahead() {
    let start_beat = 4.0;

    /// Each frame's beat from its block's `Env`, whole beats then the
    /// fraction.
    struct BeatPorts;
    impl tutti_graph::Node for BeatPorts {
        fn shape(&self) -> tutti_graph::Shape {
            tutti_graph::Shape::audio(ChannelLayout::EMPTY, ChannelLayout::STEREO)
                .with_tail(tutti_types::Tail::Unbounded)
        }
        fn prepare(&mut self, _: &tutti_graph::Prepare) {}
        fn process(
            &mut self,
            cx: &tutti_graph::Cx<'_>,
            mut io: tutti_graph::Io<'_>,
        ) -> tutti_graph::Status {
            cx.env.for_each_beat(|i, beat| {
                io.output(0)[i] = beat.floor().get() as f32;
                io.output(1)[i] = beat.fract().get() as f32;
            });
            tutti_graph::Status::Modified
        }
        fn reset(&mut self) {}
    }

    // A graph that emits the beat as its output.
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let clock = g.add(tutti_graph::Unforkable(BeatPorts));
    g.connect_output(clock, 0, 0).connect_output(clock, 1, 1);
    let (editor, executor) = g.build(RenderGraph::prepare(RATE)).expect("builds");
    let mut graph = RenderGraph::new(editor, executor).expect("built together");

    let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: Beat(start_beat),
        tempo: Bpm(120.0),
        sample_rate: RATE,
        loop_range: None,
    });

    let frames = render(&mut graph, &timeline, 2, 512);
    let f = frames.first().expect("rendered at least one frame");
    let emitted = f64::from(f[0]) + f64::from(f[1]);
    assert!(
        (emitted - start_beat).abs() < 1e-6,
        "first frame should carry the start beat {start_beat}, got {emitted}"
    );

    // After N frames the clock must have advanced by exactly N. A priming
    // advance shows up here as N+1 — the desync this test exists to catch.
    let expected = start_beat + (timeline.beats_per_sample() * 512.0).get();
    assert!(
        (timeline.beat().get() - expected).abs() < 1e-9,
        "clock advanced {} frames' worth, expected exactly 512",
        (timeline.beat().get() - start_beat) / timeline.beats_per_sample().get()
    );
}

/// A plane set narrower than the requested width zero-fills the tail —
/// and does so on EVERY block, not just the first. `drive` reuses one
/// buffer, so a `PlaneSource` that skipped the missing channels would leak
/// the previous block's samples into them.
#[test]
fn plane_source_zero_fills_a_missing_channel_every_block() {
    let planes = vec![vec![1.0f32; 64]];
    let mut src = PlaneSource::new(&planes);
    let mut out = vec![9.9f32; 4 * 4];
    let quad = ChannelLayout::from(4u16);
    assert_eq!(src.fill(InterleavedMut::new(&mut out, quad)), 4);
    for f in out.as_chunks::<4>().0 {
        assert_eq!(f, &[1.0, 0.0, 0.0, 0.0]);
    }
    // Second pull into the same dirty buffer.
    out.fill(9.9);
    assert_eq!(src.fill(InterleavedMut::new(&mut out, quad)), 4);
    for f in out.as_chunks::<4>().0 {
        assert_eq!(f, &[1.0, 0.0, 0.0, 0.0]);
    }
}
