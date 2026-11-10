//! `Graph::open_event_inlet` and `add_event_inlet`: MIDI a thread of your own
//! pushes into the session's graph, each event on its frame.
//!
//! The graph's suite (`tutti-graph/tests/event_inlet.rs`) holds the node:
//! offsets across block sizes, late, the counts, the rate, the silent fork.
//! This is the session's half: the open inserts an unwired node and marks the
//! graph edited, what is pushed is heard where the node is wired (here by an
//! event tap on its port), several inlets are independent, a prebuilt node is
//! added the same way, and, unlike the audio inlet, there is no rate to refuse
//! and an export fork is accepted.
//!
//! Mutations (run, recorded here): `Graph::open_event_inlet` returning the
//! writer without inserting the node (`pushed_events_come_out_at_their_frames`:
//! the tap cannot open, the `expect` fails); `add_event_inlet` not inserting
//! through `insert` (the same test, through `add_event_inlet`); an
//! `IntoNode` with `fork: None` (`an_export_fork_is_accepted`: the fork is
//! refused naming the inlet).

use tutti_graph::{Cx, ForkByClone, Io, Node, Prepare, Shape, Status};
use tutti_graph::{EventInletNode, EventTapReader, Refused, TapAlign, TapEvent};
use tutti_session::{EventSource, GraphSource, Session};
use tutti_types::ChannelLayout;
use tutti_types::{At, Frame, SampleRate};
use tutti_ump::Ump;

fn session() -> Session {
    Session::builder()
        .outputs_floor(1)
        .build_detached(SampleRate(48_000.0))
        .expect("builds")
}

fn note(n: u32) -> Ump {
    Ump([0x2090_0000 | n << 8 | 100, 0, 0, 0])
}

fn frames(n: usize, s: &mut Session) {
    let mut out = [0.0f32];
    for _ in 0..n {
        s.render_frame(&mut out);
    }
}

fn drain(tap: &mut EventTapReader) -> Vec<(u64, u32)> {
    let mut buf = [TapEvent {
        frame: Frame(0),
        ump: Ump::default(),
    }; 64];
    let n = tap.pop_into(&mut buf);
    buf[..n]
        .iter()
        .map(|e| (e.frame.get(), (e.ump.0[0] >> 8) & 0x7f))
        .collect()
}

/// Events pushed to an inlet come out of its port on their frames, through
/// both ways of adding one, and two inlets are independent; opening marks the
/// graph edited, and a producer needs no rate.
#[test]
fn pushed_events_come_out_at_their_frames() {
    let mut s = session();
    s.update();
    assert!(!s.graph().is_edited());
    let (key, mut writer) = s.graph().open_event_inlet(64);
    assert!(s.graph().is_edited(), "the open inserted a node");
    let (node, mut prebuilt) = EventInletNode::pair(64);
    let key2 = s.graph().add_event_inlet(node);
    assert_ne!(key, key2);
    let mut tap = s
        .graph()
        .open_event_tap(EventSource::new(key, 0), TapAlign::Raw, 64)
        .expect("the port exists");
    let mut tap2 = s
        .graph()
        .open_event_tap(EventSource::new(key2, 0), TapAlign::Raw, 64)
        .expect("the port exists");
    s.update();

    writer.push(At::Frame(Frame(10)), note(1)).unwrap();
    writer.push(At::Frame(Frame(300)), note(2)).unwrap();
    writer.push(At::NextBlock, note(3)).unwrap();
    prebuilt.push(At::Frame(Frame(10)), note(9)).unwrap();
    frames(400, &mut s);
    let first = drain(&mut tap);
    assert_eq!(first.len(), 3);
    assert!(first.contains(&(10, 1)) && first.contains(&(300, 2)));
    assert_eq!(drain(&mut tap2), [(10, 9)], "the other inlet's own events");
    assert_eq!(
        writer.push(At::Beat(tutti_types::Beat(1.0)), note(4)),
        Err(Refused::Beat)
    );
    assert_eq!((writer.lost(), writer.late(), writer.evicted()), (0, 0, 0));
}

/// A node that hears one event port and makes a sound of it, so an inlet wired
/// into it sits on the path to the output and a fork must carry it.
#[derive(Clone)]
struct Hear;

impl Node for Hear {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let n = io.events(0).len() as f32;
        io.output(0).fill(n);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// An export fork of a graph whose output is fed by an event inlet is
/// accepted, where the audio inlet's is refused naming its key, and the fork
/// is silent: it hears none of what the live graph is pushed.
///
/// Mutation (run, recorded here): an `IntoNode` with `fork: None` makes
/// `fork_live` fail with the inlet's key; a fork sharing the ring would hear
/// the push.
#[test]
fn an_export_fork_is_accepted_and_silent() {
    let mut s = session();
    let (key, mut writer) = s.graph().open_event_inlet(8);
    let (hear, ()) = s.graph().insert(ForkByClone(Hear));
    s.graph()
        .set_event_sources(hear, 0, &[EventSource::new(key, 0)]);
    s.graph().set_output_source(0, GraphSource::Node(hear, 0));
    s.update();
    writer.push(At::NextBlock, note(1)).unwrap();
    let mut fork = s.graph_ref().fork_live().expect("an inlet forks");
    let mut out = [0.0f32];
    let mut live = 0.0f32;
    for _ in 0..64 {
        fork.tick(&[], &mut out);
        assert_eq!(out[0], 0.0, "the fork hears nothing");
        s.render_frame(&mut out);
        live += out[0];
    }
    assert!(live > 0.0, "the live graph hears the push");
}

/// A capacity of 0 is refused, loudly, where the ring would be built.
#[test]
#[should_panic(expected = "an event inlet holds")]
fn a_zero_capacity_is_refused() {
    let mut s = session();
    let _ = s.graph().open_event_inlet(0);
}
