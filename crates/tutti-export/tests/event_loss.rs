//! A render that dropped events says so: an error by default (and no file
//! left behind), a count when the caller allows it.
//!
//! The graph is a node declaring a port of 4 events and writing 10 note-ons
//! every block into a node that discards them, beside a tone. A render of
//! 2,048 frames is two 1,024-frame blocks: 6 note-ons refused in each, 12 in
//! all.

use tutti_export::{
    Error, EventLossPolicy, ExportConfig, FrozenClock, RenderConfig, RenderGraph,
    render_to_buffers, render_to_file,
};
use tutti_graph::{Cx, Event, GraphBuilder, Io, Node, Prepare, Shape, Status};
use tutti_nodes::testing::Osc;
use tutti_types::{ChannelLayout, Hz, SampleRate};

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
                .push(Event::midi(at, [0x2090_3c64, 0, 0, 0]));
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

const RATE: SampleRate = SampleRate(48_000.0);

fn graph() -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let tone = g.add(Osc::sine(Hz(440.0)));
    let burst = g.add(tutti_graph::ForkByClone(Burst));
    let sink = g.add(tutti_graph::ForkByClone(Discard));
    g.connect_events(burst, 0, sink, 0);
    g.pipe_output(tone);
    let (editor, executor) = g.build(RenderGraph::prepare(RATE)).expect("builds");
    RenderGraph::new(editor, executor).expect("paired")
}

fn config(policy: EventLossPolicy) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: RATE,
            duration_seconds: 2_048.0 / 48_000.0,
            on_event_loss: policy,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// By default the render is an error naming how many were dropped.
///
/// Mutation: drop the `on_loss == Fail` check in `with_source` → the render
/// returns `Ok` → fails. Mutation: compare the executor's total, not its
/// change over the render, where the graph was reused → covered by the
/// reused-graph assertion below.
#[test]
fn a_render_that_dropped_events_fails_by_default() {
    let err = render_to_buffers(graph(), &config(EventLossPolicy::Fail), &FrozenClock)
        .expect_err("events were dropped");
    assert!(matches!(err, Error::EventsDropped { count: 12 }), "{err:?}");
}

/// Allowed, it succeeds and carries the figure; a graph rendered a second
/// time reports that render's drops, not the total.
///
/// Mutation: report `dropped_event_count()` without subtracting the count
/// before the render → the second render reads 24 → fails.
#[test]
fn an_allowed_loss_is_reported() {
    let mut g = graph();
    let first =
        render_to_buffers(&mut g, &config(EventLossPolicy::Allow), &FrozenClock).expect("allowed");
    assert_eq!(first.dropped_events, 12);
    let second =
        render_to_buffers(&mut g, &config(EventLossPolicy::Allow), &FrozenClock).expect("allowed");
    assert_eq!(second.dropped_events, 12);
}

/// A file of a failed render is not left behind; an allowed one carries the
/// count.
///
/// Mutation: drop the `remove_file` in `render_to_file` → the file stays →
/// fails.
#[test]
fn a_failed_file_render_leaves_no_file() {
    let dir = std::env::temp_dir().join(format!("tutti-export-loss-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("out.wav");
    let err = render_to_file(graph(), &config(EventLossPolicy::Fail), &FrozenClock, &path)
        .expect_err("events were dropped");
    assert!(matches!(err, Error::EventsDropped { count: 12 }), "{err:?}");
    assert!(!path.exists());
    let written = render_to_file(
        graph(),
        &config(EventLossPolicy::Allow),
        &FrozenClock,
        &path,
    )
    .expect("allowed");
    assert_eq!(written.dropped_events, 12);
    assert!(path.exists());
    std::fs::remove_dir_all(&dir).unwrap();
}
