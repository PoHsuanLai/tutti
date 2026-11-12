//! Event ports on typed keys (doc 013, "The session graph's surface"; doc 030
//! step 2c): `key.events_in()` and `key.events_out()` are the main (index 0)
//! event ports, `events_in_at(i)` and `events_out_at(i)` take an index, and
//! the one `connect`, `disconnect`, `sources`, `set_sources` and
//! `disconnect_all` cover an event edge as they do an audio one.
//!
//! The figures are the graph's own: which edges it holds.

use tutti_graph::{Cx, ForkByClone, Io, Node, Prepare, Shape, Status};
use tutti_session::{ConnectError, EventInPort, EventOutPort, Session};
use tutti_types::{ChannelLayout, SampleRate};

/// A node with `IN` event inputs and `OUT` event outputs and nothing else.
#[derive(Clone)]
struct Events<const IN: u16, const OUT: u16>;

impl<const IN: u16, const OUT: u16> Node for Events<IN, OUT> {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).with_events(IN, OUT)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn detached() -> Session {
    Session::builder()
        .outputs_floor(1)
        .build_detached(SampleRate(48_000.0))
        .expect("builds")
}

/// **An event edge is connected, read back and disconnected by the same calls
/// as an audio one**: `connect` adds it (once, however often it is asked
/// for), `sources` lists what feeds the port in the graph's order, and
/// `disconnect` says whether it was there.
#[test]
fn an_event_edge_is_connected_read_back_and_disconnected() {
    let mut s = detached();
    let g = s.graph();
    let (src, _) = g.insert(ForkByClone(Events::<0, 2>));
    let (sink, _) = g.insert(ForkByClone(Events::<1, 0>));

    let connection = g.connect(src.events_out(), sink.events_in()).unwrap();
    assert!(connection.edges().is_empty(), "an event edge is no audio edge");
    g.connect(src.events_out(), sink.events_in()).unwrap();
    assert_eq!(g.sources(sink.events_in()), [src.events_out()], "once");

    g.connect(src.events_out_at(1), sink.events_in()).unwrap();
    assert_eq!(
        g.sources(sink.events_in()),
        [src.events_out(), src.events_out_at(1)],
        "events merge: a second source joins the first"
    );

    assert!(g.disconnect(src.events_out(), sink.events_in()));
    assert!(!g.disconnect(src.events_out(), sink.events_in()), "gone");
    assert_eq!(g.sources(sink.events_in()), [src.events_out_at(1)]);
}

/// **Port 0 is the main event port, and an index names another**: an edge
/// onto input 1 is no source of input 0.
#[test]
fn event_port_zero_is_the_main_port_and_an_index_names_another() {
    let mut s = detached();
    let g = s.graph();
    let (src, _) = g.insert(ForkByClone(Events::<0, 1>));
    let (sink, _) = g.insert(ForkByClone(Events::<2, 0>));
    assert_eq!(sink.events_in(), sink.events_in_at(0));
    assert_eq!(src.events_out(), src.events_out_at(0));
    assert_eq!(sink.events_in().index(), 0);

    g.connect(src.events_out(), sink.events_in_at(1)).unwrap();
    assert!(g.sources(sink.events_in()).is_empty(), "the main input is unfed");
    assert_eq!(g.sources(sink.events_in_at(1)), [src.events_out()]);
}

/// **A port the node does not have, a node that is not there, and a node
/// feeding itself are refused with the port named, and nothing is stored.**
#[test]
fn a_missing_event_port_is_refused_and_stores_nothing() {
    let mut s = detached();
    let g = s.graph();
    let (src, _) = g.insert(ForkByClone(Events::<0, 1>));
    let (sink, _) = g.insert(ForkByClone(Events::<1, 0>));
    let (both, _) = g.insert(ForkByClone(Events::<1, 1>));
    let (gone, _) = g.insert(ForkByClone(Events::<1, 1>));
    g.remove(gone);
    let _ = s.update();
    let g = s.graph();

    assert_eq!(
        g.connect(src.events_out(), sink.events_in_at(1)).err(),
        Some(ConnectError::NoSuchEventIn(sink.events_in_at(1)))
    );
    assert_eq!(
        g.connect(src.events_out_at(1), sink.events_in()).err(),
        Some(ConnectError::NoSuchEventOut(src.events_out_at(1)))
    );
    // A sink with no event output has no output 0 either.
    assert_eq!(
        g.connect(sink.events_out(), src.events_in()).err(),
        Some(ConnectError::NoSuchEventOut(sink.events_out()))
    );
    assert_eq!(
        g.connect(both.events_out(), both.events_in()).err(),
        Some(ConnectError::Itself(both.erase()))
    );
    assert_eq!(
        g.connect(src.events_out(), gone.events_in()).err(),
        Some(ConnectError::NoSuchNode(gone.erase()))
    );
    assert!(!s.graph_ref().is_edited(), "a refusal stores nothing");
}

/// **`set_sources` sets exactly what feeds an event input, and
/// `disconnect_all` takes it all**: a source listed is kept, one not listed
/// goes, an empty set clears.
#[test]
fn set_sources_and_disconnect_all_cover_event_inputs() {
    let mut s = detached();
    let g = s.graph();
    let (a, _) = g.insert(ForkByClone(Events::<0, 1>));
    let (b, _) = g.insert(ForkByClone(Events::<0, 1>));
    let (sink, _) = g.insert(ForkByClone(Events::<1, 0>));
    let input = sink.events_in();

    g.set_sources(input, [a.events_out(), b.events_out()]).unwrap();
    let mut held = g.sources(input);
    held.sort();
    let mut both = [a.events_out(), b.events_out()];
    both.sort();
    assert_eq!(held, both);
    g.set_sources(input, [b.events_out()]).unwrap();
    assert_eq!(g.sources(input), [b.events_out()]);

    assert!(g.disconnect_all(input));
    assert!(g.sources(input).is_empty());
    assert!(!g.disconnect_all(input), "nothing left to take");

    // The refusal names the port and changes nothing.
    assert_eq!(
        g.set_sources(input, [EventOutPort::new(a, 5)]),
        Err(ConnectError::NoSuchEventOut(EventOutPort::new(a, 5)))
    );
    assert_eq!(
        g.set_sources(EventInPort::new(sink, 3), [a.events_out()]),
        Err(ConnectError::NoSuchEventIn(EventInPort::new(sink, 3)))
    );
    assert!(g.sources(input).is_empty());
}
