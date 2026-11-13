//! An event tap opened on a session's graph: the MIDI events a node's port
//! carries, each with its frame, copied out for a thread of the caller's.
//!
//! The graph's own suite (`tutti-graph/tests/event_tap.rs`) holds the op: frames
//! across block sizes, alignment, loss, a missing port, the parallel render.
//! This is the session's half: the open marks the graph edited, a close stops
//! the copy, a reader dropped first is closed by the `Reclaim` phase, and the
//! taps are opened again at the graph's new rate.

use tutti_graph::{
    Cx, Event, EventCapacity, Io, MissingSource, Node, Op, Prepare, Shape, Status, TapAlign,
    TapEvent, Unforkable,
};
use tutti_session::{CloseError, EventInPort, EventOutPort, Session};
use tutti_types::ChannelLayout;
use tutti_types::{Frame, SampleRate};
use tutti_ump::Ump;

/// A note-on on every frame that is a multiple of 4, its own frame in word 0.
struct Every4;

impl Node for Every4 {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(0, 1)
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

fn session() -> (Session, EventOutPort) {
    let mut s = Session::builder()
        .outputs_floor(ChannelLayout::MONO)
        .build_detached(SampleRate::SR_48K)
        .expect("builds");
    let (key, ()) = s.graph().add(Unforkable(Every4));
    (s, key.events_out())
}

fn frames(n: usize, s: &mut Session) {
    let mut out = [0.0f32];
    for _ in 0..n {
        s.render_frame(&mut out);
    }
}

fn drain(tap: &mut tutti_graph::EventTapReader) -> Vec<u64> {
    let mut buf = [TapEvent {
        frame: Frame::new(0),
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
        .testing()
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
        .taps()
        .open_event(source, TapAlign::Raw, EventCapacity::of(64))
        .expect("the port exists");
    assert_eq!(tap.rate(), SampleRate::SR_48K);
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
            .taps()
            .open_event(
                EventOutPort::new(source.node(), 3),
                TapAlign::Raw,
                EventCapacity::of(8)
            )
            .err(),
        Some(MissingSource::EventPort {
            node: source.node(),
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
        .taps()
        .open_event(source, TapAlign::Raw, EventCapacity::of(64))
        .expect("the port exists");
    frames(6, &mut s);
    assert_eq!(tap_ops(&s), 1);
    s.graph().taps().close_event(tap.id()).unwrap();
    assert_eq!(
        s.graph().taps().close_event(tap.id()),
        Err(CloseError::EventTap(tap.id())),
        "a closed event tap is not open"
    );
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
        .taps()
        .open_event(source, TapAlign::Raw, EventCapacity::of(64))
        .expect("the port exists");
    let _ = s.update();
    assert_eq!(tap_ops(&s), 1, "one op a tap while it is open");
    drop(tap);
    let _ = s.update();
    assert_eq!(
        tap_ops(&s),
        1,
        "the reclaim of this update closed it; the plan is the last commit's"
    );
    let _ = s.update();
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
        .taps()
        .open_event(source, TapAlign::Raw, EventCapacity::of(64))
        .expect("the port exists");
    frames(5, &mut s);
    assert_eq!(drain(&mut old), vec![0, 4]);
    s.graph().set_sample_rate(SampleRate::SR_44K1);
    let mut readers = s.graph().taps().reopen_events();
    assert_eq!(readers.len(), 1);
    let mut new = readers.remove(0);
    assert_eq!(new.id(), old.id());
    assert_eq!(old.rate(), SampleRate::SR_48K);
    assert_eq!(new.rate(), SampleRate::SR_44K1);
    frames(16, &mut s);
    assert_eq!(
        drain(&mut old),
        Vec::<u64>::new(),
        "the superseded reader is fed no more"
    );
    assert!(!drain(&mut new).is_empty(), "the new reader is");
}

/// A node with one event input that does nothing with it.
struct Hears;

impl Node for Hears {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// A tap opened on an event input (`key.events_in()`) copies what the node
/// hears there: the events its source wrote, on their frames, and the tap
/// survives a re-rate through `reopen_events`. A port past the node's inputs
/// is refused as an input.
#[test]
fn a_tap_of_an_event_input_hears_what_the_node_hears() {
    let (mut s, source) = session();
    let (hears, ()) = s.graph().add(Unforkable(Hears));
    s.graph()
        .connect(source, hears.events_in())
        .expect("both ports exist");
    assert_eq!(
        s.graph()
            .taps()
            .open_event(
                EventInPort::new(hears.erase(), 1),
                TapAlign::Raw,
                EventCapacity::of(8)
            )
            .err(),
        Some(MissingSource::EventInPort {
            node: hears.erase(),
            port: 1,
            width: 1
        })
    );
    let mut tap = s
        .graph()
        .taps()
        .open_event(hears.events_in(), TapAlign::Raw, EventCapacity::of(64))
        .expect("the input exists");
    frames(16, &mut s);
    assert_eq!(drain(&mut tap), vec![0, 4, 8, 12]);
    s.graph().set_sample_rate(SampleRate::SR_44K1);
    let mut readers = s.graph().taps().reopen_events();
    assert_eq!(readers.len(), 1);
    let mut new = readers.remove(0);
    assert_eq!(new.rate(), SampleRate::SR_44K1);
    frames(8, &mut s);
    assert!(
        !drain(&mut new).is_empty(),
        "the reopened tap still hears it"
    );
}
