use super::*;
use crate::GRAPH_MAX_BLOCK;
use crate::control::{Pass, Progress};
use tutti_graph::GraphBuilder;
use tutti_transport::{FrozenClock, OfflineTimeline, OfflineTimelineConfig};
use tutti_types::ChannelLayout;
use tutti_types::{Beat, Bpm, SampleRate};
use tutti_ump::Ump;

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
    let file = FileChannels {
        layout: ChannelLayout::try_from(ch).expect("a test width is small"),
        topology: None,
    };
    with_source(graph, RATE, clock, &file, |src| {
        drive(
            src,
            ChannelLayout::try_from(ch).expect("a test width is small"),
            &plan,
            &mut Control::new(Pass::ONLY, &mut crate::control::never_stop),
            crate::EventLossPolicy::default(),
            |b| {
                out.extend(b.iter_frames().map(<[f32]>::to_vec));
                Ok(())
            },
        )
    })
    .unwrap()
    .0
    .finished()
    .expect("not cancelled");
    out
}

/// Drive `frames` of `mono_dc` ticking `tick`; returns the outcome and the
/// frames consumed.
fn drive_ticked(frames: usize, tick: &mut dyn FnMut(Progress) -> Flow) -> (Outcome<()>, usize) {
    let plan = crate::render::RenderPlan {
        total: Samples(frames),
        output_length: Samples(frames),
        latency: Samples(0),
    };
    let mut consumed = 0;
    let file = FileChannels {
        layout: ChannelLayout::MONO,
        topology: None,
    };
    let outcome = with_source(&mut mono_dc(0.5), RATE, &FrozenClock, &file, |src| {
        drive(
            src,
            ChannelLayout::MONO,
            &plan,
            &mut Control::new(Pass::ONLY, tick),
            crate::EventLossPolicy::default(),
            |b| {
                consumed += b.iter_frames().count();
                Ok(())
            },
        )
    })
    .unwrap()
    .0;
    (outcome, consumed)
}

/// A tick that stops after block `n` ends the drive there, as `Cancelled`,
/// having consumed exactly `n` blocks and no more.
///
/// Mutation (run): `drive` ignoring the tick's answer -> it renders all
/// 16 blocks and returns `Finished`; fails.
/// Mutation (run): `drive` ticking before it consumes the block -> the
/// consumed count is one block short; fails.
#[test]
fn a_tick_that_stops_ends_the_drive_after_that_block() {
    let block = GRAPH_MAX_BLOCK.get();
    let mut ticks = 0;
    let (outcome, consumed) = drive_ticked(16 * block, &mut |_| {
        ticks += 1;
        if ticks == 3 {
            Flow::Stop
        } else {
            Flow::Continue
        }
    });
    assert_eq!(outcome, Outcome::Cancelled);
    assert_eq!(ticks, 3);
    assert_eq!(consumed, 3 * block);
}

/// Progress rises strictly block by block, names its pass, carries the plan's
/// total, and the last reading is the total.
///
/// Mutation (run): `Control::report` given `Samples(0)` for produced -> not
/// rising; fails.
#[test]
fn progress_rises_and_reaches_the_total() {
    let total = 5 * GRAPH_MAX_BLOCK.get() + 17;
    let mut seen = Vec::new();
    let (outcome, _) = drive_ticked(total, &mut |p| {
        seen.push(p);
        Flow::Continue
    });
    assert_eq!(outcome, Outcome::Finished(()));
    assert_eq!(seen.len(), 6);
    assert!(seen.windows(2).all(|w| w[0].produced < w[1].produced));
    assert!(
        seen.iter()
            .all(|p| p.total == Samples(total) && p.pass == Pass::ONLY)
    );
    assert_eq!(seen.last().unwrap().produced, Samples(total));
}

/// A mono graph rendered to a wider file must NOT be copied into every
/// channel: it is the centre of a 5.1 file, and a full-level mono copy in the
/// LFE and surrounds is the bug this pins.
///
/// Mutation (run): the fold replaced by `dst.fill(src[0])` for a one-channel
/// source → channel 1 reads 0.5.
#[test]
fn a_mono_graph_does_not_fill_every_surround_channel() {
    let frames = render(&mut mono_dc(0.5), &FrozenClock, 6, 64);
    assert!(!frames.is_empty());
    for f in &frames {
        assert_eq!(f.len(), 6);
        for (c, &s) in f.iter().enumerate() {
            let want = if c == 2 { 0.5 } else { 0.0 };
            assert_eq!(s, want, "channel {c}: the mono signal is the centre only");
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
/// Mutation (run): the fold built for the file's width as its source width
/// → it indexes a missing plane and panics.
#[test]
fn rendering_a_narrow_graph_to_a_wide_file_does_not_panic() {
    for ch in [2usize, 3, 4, 5, 12] {
        let frames = render(&mut mono_dc(0.25), &FrozenClock, ch, 32);
        assert_eq!(frames.len(), 32, "width {ch}");
        assert!(frames.iter().all(|f| f.len() == ch), "width {ch}");
    }
}

/// The planes follow the committed plan, not the editor's spec: outputs
/// narrowed in the spec with no `commit()` leave the executor's plan as wide
/// as it was, and the render must carry every channel of it (an undersized
/// plane list made the executor render silence in release).
///
/// Mutation (run): size `planes` from `editor.spec().topology.outputs`
/// again → the plan is wider than the planes; the debug build asserts and
/// release renders silence → fails.
#[test]
fn a_spec_narrowed_without_a_commit_still_renders_the_committed_width() {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let k = g.add(tutti_nodes::testing::Const::mono(0.5));
    g.pipe_output(k);
    let (editor, executor) = g.build(RenderGraph::prepare(RATE)).expect("builds");
    let mut graph = RenderGraph::new(editor, executor).expect("built together");
    let committed = graph.editor().spec().topology.outputs.len();
    assert!(committed >= 2, "a stereo graph has two outputs");
    graph.editor_mut().spec_mut().topology.outputs.truncate(1);
    let frames = render(&mut graph, &FrozenClock, 2, 16);
    assert!(frames.iter().all(|f| f[0] == 0.5 && f[1] == 0.5));
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
        tempo: Bpm::of(120.0),
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
    let mut src = PlaneSource::new(&planes, None);
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

/// A recovery of a refusal from before the render does not mask a loss in it:
/// one refusal owed at the start, then one new refusal and one recovery (the
/// older one, as the clip sends owed note-offs first): one event is lost. The
/// old subtraction of `lost_event_count`s read 0.
///
/// Mutation: ignore `owed` in `lost_over` → 0 → fails.
#[test]
fn a_recovery_of_an_older_refusal_does_not_mask_a_new_loss() {
    assert_eq!(lost_over((1, 0), (2, 1)), 1);
    // The new refusal recovered too: nothing lost.
    assert_eq!(lost_over((1, 0), (2, 2)), 0);
    // Nothing owed: a recovery in the span is of a refusal in it.
    assert_eq!(lost_over((4, 4), (6, 5)), 1);
    assert_eq!(lost_over((0, 0), (3, 0)), 3);
}

mod loss {
    use super::*;
    use tutti_graph::{Cx, Event, ForkByClone, Io, Node, Prepare, Shape, Status};

    /// Ten note-ons into a port of four, every block: six refused.
    #[derive(Clone)]
    struct Burst;

    impl Node for Burst {
        fn shape(&self) -> Shape {
            Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
                .with_events(0, 1)
                .with_event_capacity(4)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
            for i in 0..10 {
                let at = cx.env.offset(i).expect("inside the block");
                let _ = io
                    .event_out(0)
                    .push(Event::midi(at, Ump([0x2090_3c64, 0, 0, 0])));
            }
            Status::Silent
        }
        fn reset(&mut self) {}
    }

    #[derive(Clone)]
    struct Discard;

    impl Node for Discard {
        fn shape(&self) -> Shape {
            Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(1, 0)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
            Status::Silent
        }
        fn reset(&mut self) {}
    }

    fn lossy() -> RenderGraph {
        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
        let k = g.add(tutti_nodes::testing::Const::mono(0.5));
        let burst = g.add(ForkByClone(Burst));
        let sink = g.add(ForkByClone(Discard));
        g.connect_events(burst, 0, sink, 0);
        g.pipe_output(k);
        let (editor, executor) = g.build(RenderGraph::prepare(RATE)).expect("builds");
        RenderGraph::new(editor, executor).expect("built together")
    }

    fn lost_in_one_block(graph: &mut RenderGraph) -> u64 {
        let plan = crate::render::RenderPlan {
            total: Samples(GRAPH_MAX_BLOCK.get()),
            output_length: Samples(GRAPH_MAX_BLOCK.get()),
            latency: Samples(0),
        };
        let file = FileChannels {
            layout: ChannelLayout::MONO,
            topology: None,
        };
        with_source(graph, RATE, &FrozenClock, &file, |src| {
            drive(
                src,
                ChannelLayout::MONO,
                &plan,
                &mut Control::new(Pass::ONLY, &mut crate::control::never_stop),
                crate::EventLossPolicy::Allow,
                |_| Ok(()),
            )
        })
        .unwrap()
        .1
    }

    /// A render counts its losses from where the graph last settled, not from
    /// the instant its source is built, so what the executor dropped in
    /// between (installing a commit can drop a note-off closing into a full
    /// delay) is in its figure; and a render settles the graph, so the next
    /// one does not count it again.
    ///
    /// Mutation (run): `with_source` handing `GraphSource::new` the executor's
    /// current counts instead of `graph.settled()` -> 6 for the second
    /// render, not 12; fails. Mutation (run): not calling `graph.settle` ->
    /// 18 for the third; fails.
    #[test]
    fn a_render_counts_what_the_graph_dropped_since_it_settled() {
        let mut graph = lossy();
        assert_eq!(lost_in_one_block(&mut graph), 6);
        // As if the 6 were dropped after the graph settled, not in the render
        // that reported them.
        graph.settle((0, 0));
        assert_eq!(lost_in_one_block(&mut graph), 12);
        assert_eq!(lost_in_one_block(&mut graph), 6, "settled by the last");
    }
}
