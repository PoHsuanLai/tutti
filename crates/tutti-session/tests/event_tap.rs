//! An event tap opened on a session's graph: the MIDI events a node's port
//! carries, each with its frame, copied out for a thread of the caller's.
//!
//! The graph's own suite (`tutti-graph/tests/event_tap.rs`) holds the op: frames
//! across block sizes, alignment, loss, a missing port, the parallel render.
//! This is the session's half: the open marks the graph edited, a close stops
//! the copy, a reader dropped first is closed by the `Reclaim` phase, and the
//! taps are opened again at the graph's new rate.
//!
//! Mutations (run, recorded here): `Graph::open_event_tap` not marking the graph
//! edited (`opening_a_tap_yields_the_events_rendered_since_the_open`: with
//! nothing else edited after the first render the reader stays empty);
//! `Graph::close_event_tap` doing nothing (`closing_the_tap_stops_the_copy`);
//! deleting `self.graph.reap_event_taps()` from the `Phase::Reclaim` arm
//! (`a_dropped_reader_leaves_the_plan_at_the_reclaim_phase`); a
//! `Graph::reopen_event_taps` that drops each reader it opens
//! (`reopening_replaces_the_tap_at_the_new_rate`: no reader comes back).

use tutti_graph::{
    Cx, Event, Io, MissingSource, Node, Op, Prepare, Shape, Status, TapAlign, TapEvent, Unforkable,
};
use tutti_session::{EventSource, Session};
use tutti_types::{ChannelLayout, Frame, SampleRate};
use tutti_ump::Ump;

/// A note-on on every frame that is a multiple of 4, its own frame in word 0.
struct Every4;

impl Node for Every4 {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(0, 1)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        for at in cx.env.offsets() {
            let f = cx.env.frame_at(at).get();
            if f.is_multiple_of(4) {
                let _ = io
                    .event_out(0)
                    .push(Event::midi(at, Ump([f as u32, 0, 0, 0])));
            }
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

fn session() -> (Session, EventSource) {
    let mut s = Session::builder()
        .outputs_floor(1)
        .build_detached(SampleRate(48_000.0))
        .expect("builds");
    let (key, ()) = s.graph().insert(Unforkable(Every4));
    (s, EventSource::new(key, 0))
}

fn frames(n: usize, s: &mut Session) {
    let mut out = [0.0f32];
    for _ in 0..n {
        s.render_frame(&mut out);
    }
}

fn drain(tap: &mut tutti_graph::EventTapReader) -> Vec<u64> {
    let mut buf = [TapEvent {
        frame: Frame(0),
        ump: Ump::default(),
    }; 64];
    let n = tap.pop_into(&mut buf);
    buf[..n]
        .iter()
        .map(|e| {
            assert_eq!(
                u64::from(e.ump.0[0]),
                e.frame.get(),
                "the packet is its own frame"
            );
            e.frame.get()
        })
        .collect()
}

fn tap_ops(s: &Session) -> usize {
    s.graph_ref()
        .sent_plan()
        .expect("a plan was sent")
        .ops()
        .iter()
        .filter(|op| matches!(op, Op::EventTap { .. }))
        .count()
}

/// The whole point of the seam: a host gets the reader from the graph and it
/// sees the events rendered since the open, each on its frame, at the graph's
/// rate.
#[test]
fn opening_a_tap_yields_the_events_rendered_since_the_open() {
    let (mut s, source) = session();
    frames(1, &mut s);
    let mut tap = s
        .graph()
        .open_event_tap(source, TapAlign::Raw, 64)
        .expect("the port exists");
    assert_eq!(tap.rate(), SampleRate(48_000.0));
    frames(15, &mut s);
    assert_eq!(drain(&mut tap), vec![4, 8, 12]);
    assert_eq!(tap.take_lost(), 0);
}

/// A port the graph does not have is refused, and nothing is opened.
#[test]
fn opening_refuses_a_port_the_graph_does_not_have() {
    let (mut s, source) = session();
    assert_eq!(
        s.graph()
            .open_event_tap(EventSource::new(source.node, 3), TapAlign::Raw, 8)
            .err(),
        Some(MissingSource::EventPort {
            node: source.node,
            port: 3,
            width: 1
        })
    );
}

/// Closing stops the copy, so a host that is done is back to a plan with no
/// tap op.
#[test]
fn closing_the_tap_stops_the_copy() {
    let (mut s, source) = session();
    let mut tap = s
        .graph()
        .open_event_tap(source, TapAlign::Raw, 64)
        .expect("the port exists");
    frames(6, &mut s);
    assert_eq!(tap_ops(&s), 1);
    s.graph().close_event_tap(tap.id());
    frames(8, &mut s);
    assert_eq!(drain(&mut tap), vec![0, 4], "only what rendered while open");
    assert_eq!(tap_ops(&s), 0);
}

/// A tap whose reader was dropped is closed by the next `Reclaim` phase and
/// leaves the plan at the commit after it.
#[test]
fn a_dropped_reader_leaves_the_plan_at_the_reclaim_phase() {
    let (mut s, source) = session();
    let tap = s
        .graph()
        .open_event_tap(source, TapAlign::Raw, 64)
        .expect("the port exists");
    s.update();
    assert_eq!(tap_ops(&s), 1, "one op a tap while it is open");
    drop(tap);
    s.update();
    assert_eq!(
        tap_ops(&s),
        1,
        "the reclaim of this update closed it; the plan is the last commit's"
    );
    s.update();
    assert_eq!(tap_ops(&s), 0, "closed");
}

/// After the graph is re-prepared at another rate the taps are opened again:
/// each new reader replaces the tap of its id, is on the new rate, and the
/// reader it supersedes is fed no more.
#[test]
fn reopening_replaces_the_tap_at_the_new_rate() {
    let (mut s, source) = session();
    let mut old = s
        .graph()
        .open_event_tap(source, TapAlign::Raw, 64)
        .expect("the port exists");
    frames(5, &mut s);
    assert_eq!(drain(&mut old), vec![0, 4]);
    s.graph().set_sample_rate(SampleRate(44_100.0));
    let mut readers = s.graph().reopen_event_taps();
    assert_eq!(readers.len(), 1);
    let mut new = readers.remove(0);
    assert_eq!(new.id(), old.id());
    assert_eq!(old.rate(), SampleRate(48_000.0));
    assert_eq!(new.rate(), SampleRate(44_100.0));
    frames(16, &mut s);
    assert_eq!(
        drain(&mut old),
        Vec::<u64>::new(),
        "the superseded reader is fed no more"
    );
    assert!(!drain(&mut new).is_empty(), "the new reader is");
}
