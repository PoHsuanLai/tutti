//! A render that dropped events says so: an error by default (and no file
//! left behind), a count when the caller allows it.
//!
//! The graph is a node declaring a port of 4 events and writing 10 note-ons
//! every block into a node that discards them, beside a tone. A render of
//! 2,048 frames is two 1,024-frame blocks: 6 note-ons refused in each, 12 in
//! all.

mod support;

use support::{render_to_buffers, render_to_file};
use tutti_export::{
    CancelToken, Control, Error, EventLossPolicy, ExportConfig, FrozenClock, Progress,
    RenderConfig, RenderGraph, write_buffers,
};
use tutti_graph::{Cx, Event, GraphBuilder, Io, Node, Prepare, Shape, Status};
use tutti_nodes::testing::Osc;
use tutti_types::Seconds;
use tutti_types::{ChannelLayout, Hz, SampleRate};
use tutti_ump::Ump;

#[derive(Clone)]
struct Burst;

impl Node for Burst {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(0, 1).with_event_capacity(4)
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
        Shape::EMPTY.with_events(1, 0)
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
            duration_seconds: Seconds(2_048.0 / 48_000.0),
            on_event_loss: policy,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// By default the render is an error naming how many were dropped, and it
/// stops at the first block that dropped any: the 6 refused in block one, not
/// the 12 of the whole render, so the progress callback never hears of the
/// second block (its reading at 2,048 frames, the plan's total, is delivered
/// whenever the block is rendered).
///
/// Mutation: drop the per-block `check_loss` in `drive` → the render runs both
/// blocks and reports 12 (and delivers the second block's reading) → fails. Mutation: compare the
/// executor's total, not its change over the render, where the graph was
/// reused → covered by the reused-graph assertion below.
#[test]
fn a_render_that_dropped_events_stops_at_the_first_block_that_did() {
    let mut rendered_frames = Vec::new();
    let token = CancelToken::new();
    let mut record = |p: &Progress| rendered_frames.push(p.frames().map(|(done, _)| done.get()));
    let err = tutti_export::render_to_buffers(
        graph(),
        &config(EventLossPolicy::Fail),
        &FrozenClock,
        &mut Control::new(&token).on_progress(&mut record),
    )
    .expect_err("events were dropped");
    assert!(matches!(err, Error::EventsDropped { count: 6 }), "{err:?}");
    assert_eq!(
        rendered_frames,
        [Some(0)],
        "only the render's start was reported: the second block was never rendered"
    );
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
    assert_eq!(first.dropped_events(), 12);
    let second =
        render_to_buffers(&mut g, &config(EventLossPolicy::Allow), &FrozenClock).expect("allowed");
    assert_eq!(second.dropped_events(), 12);
}

/// A render that lost events puts nothing in place: the file already at the
/// path is byte for byte what it was, and no temporary is left beside it, on
/// the streaming path and the normalizing one. An allowed render replaces it
/// and carries the count. A render the caller cancels is not judged.
///
/// Mutation: move the `check_loss` in `encode_to_file` after `staged.commit`
/// (or remove it) → the old file is replaced → fails. Mutation: the same
/// check in `render_pass` removed → the normalizing path writes the damaged
/// render → fails.
#[test]
fn a_render_that_lost_events_replaces_nothing() {
    let dir = std::env::temp_dir().join(format!("tutti-export-loss-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("out.wav");
    let before = b"the previous export".to_vec();
    std::fs::write(&path, &before).unwrap();
    let leftovers = || {
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };

    let err = render_to_file(graph(), &config(EventLossPolicy::Fail), &FrozenClock, &path)
        .expect_err("events were dropped");
    assert!(matches!(err, Error::EventsDropped { count: 6 }), "{err:?}");
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(leftovers(), vec!["out.wav".to_string()], "no temporary");

    let normalize = tutti_export::Normalize::Peak {
        target: tutti_types::Db(-1.0),
    };
    let err = support::render_normalized_to_file(
        graph(),
        &config(EventLossPolicy::Fail),
        &FrozenClock,
        normalize,
        &path,
    )
    .expect_err("events were dropped");
    assert!(matches!(err, Error::EventsDropped { count: 6 }), "{err:?}");
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(leftovers(), vec!["out.wav".to_string()], "no temporary");

    // Cancelled before the first block: not an error, and still untouched.
    let token = CancelToken::new();
    token.cancel();
    let cancelled = tutti_export::render_to_file(
        graph(),
        &config(EventLossPolicy::Fail),
        &FrozenClock,
        &path,
        &mut Control::new(&token),
    )
    .expect("a cancelled render is not an error");
    assert_eq!(cancelled, tutti_export::Outcome::Cancelled);
    assert_eq!(std::fs::read(&path).unwrap(), before);

    let written = render_to_file(
        graph(),
        &config(EventLossPolicy::Allow),
        &FrozenClock,
        &path,
    )
    .expect("allowed");
    assert_eq!(written.dropped_events, 12);
    assert_ne!(std::fs::read(&path).unwrap(), before);
    assert_eq!(leftovers(), vec!["out.wav".to_string()]);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Planes that carry a loss are judged by the config they are written with:
/// `Fail` refuses them and leaves the file alone, `Allow` writes them and
/// reports the count.
///
/// Mutation: drop the `check_loss` in `encode_planes` → the write under `Fail`
/// succeeds and replaces the file → fails.
#[test]
fn write_buffers_applies_the_event_loss_policy() {
    let rendered =
        render_to_buffers(graph(), &config(EventLossPolicy::Allow), &FrozenClock).expect("allowed");
    assert_eq!(rendered.dropped_events(), 12);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.wav");
    std::fs::write(&path, b"old").unwrap();
    let err = write_buffers(&rendered, &config(EventLossPolicy::Fail), &path)
        .expect_err("the planes lost events");
    assert!(matches!(err, Error::EventsDropped { count: 12 }), "{err:?}");
    assert_eq!(std::fs::read(&path).unwrap(), b"old");

    let written =
        write_buffers(&rendered, &config(EventLossPolicy::Allow), &path).expect("allowed");
    assert_eq!(written.dropped_events, 12);
}
