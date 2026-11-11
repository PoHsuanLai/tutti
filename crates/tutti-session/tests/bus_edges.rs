//! The session's bus edges (doc 013, "Buses and coercion between declared
//! layouts", B2): `Graph::connect_bus`, `disconnect_bus`, `bus_sources` and
//! `buses`, and what a bus edge does to the sum its sink holds.
//!
//! Every figure is analytic: constants of exactly representable values, so
//! `0.5 L + 0.5 R` is exact in `f32`.

use tutti_graph::{BusError, BusRef, BusSource, Buses, NodeKey, Unforkable};
use tutti_graph::{Cx, Io, Node, Prepare, Shape, Status};
use tutti_session::{EdgeGainError, EdgeRef, GraphSource, Session};
use tutti_types::{
    Amplitude, BusTopology, ChannelLayout, ChannelTopology, FoldError, InterleavedMut, SampleRate,
    Samples,
};

const BLOCK: usize = 64;

fn offline() -> Session {
    Session::builder()
        .build_offline(SampleRate(48_000.0), ChannelLayout::STEREO, Samples(1_024))
        .expect("builds")
}

fn layout(n: u16) -> ChannelLayout {
    ChannelLayout::from_count(n)
}

fn named(n: u16) -> BusTopology {
    BusTopology::try_from(&ChannelTopology::smpte(layout(n)).expect("an SMPTE width")).unwrap()
}

fn discrete(n: u16) -> BusTopology {
    BusTopology::discrete(layout(n)).unwrap()
}

/// Constant `values` on its output channels, declaring `out` as its bus.
struct Src {
    values: Vec<f32>,
    out: BusTopology,
}

impl Node for Src {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, layout(self.values.len() as u16))
            .with_buses_out(Buses::main(self.out))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for (c, &v) in self.values.iter().enumerate() {
            io.output(c)[0] = v;
        }
        Status::Constant
    }
    fn reset(&mut self) {}
}

/// A pass-through of its input, declaring `input` as its bus.
struct Pass {
    input: BusTopology,
}

impl Node for Pass {
    fn shape(&self) -> Shape {
        let w = self.input.layout();
        Shape::audio(w, w).with_buses_in(Buses::main(self.input))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Bypass
    }
    fn reset(&mut self) {}
}

/// Commits and renders a block; returns output channel 0.
fn out0(s: &mut Session) -> Vec<f32> {
    s.update();
    let mut block = vec![f32::NAN; BLOCK * 2];
    s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO));
    block.iter().step_by(2).copied().collect()
}

fn all_are(block: &[f32], want: f32) -> bool {
    block.iter().all(|x| x.to_bits() == want.to_bits())
}

fn bus(node: NodeKey) -> BusRef {
    BusRef { node, bus: 0 }
}

struct Rig {
    s: Session,
    stereo: NodeKey,
    own: NodeKey,
    sink: NodeKey,
}

/// A stereo source (`0.25`, `0.75`), a mono source of `0.125`, and a mono
/// pass-through feeding output 0, committed once.
fn rig() -> Rig {
    let mut s = offline();
    let g = s.graph();
    let (stereo, _) = g.add(Unforkable(Src {
        values: vec![0.25, 0.75],
        out: named(2),
    }));
    let (own, _) = g.add(Unforkable(Src {
        values: vec![0.125],
        out: named(1),
    }));
    let (sink, _) = g.add(Unforkable(Pass { input: named(1) }));
    g.set_output_source(0, GraphSource::Node(sink, 0));
    s.update();
    assert!(!s.graph_ref().is_edited());
    Rig {
        s,
        stereo,
        own,
        sink,
    }
}

/// Connect, render, disconnect: the fold sums beside an ordinary source of the
/// sink's port while it is connected, and removing it leaves the ordinary
/// source alone. The declared buses read back.
///
/// Mutations (run), each failing this test: `connect_bus` not marking the
/// graph edited (the first render is silent); `disconnect_bus` removing the
/// sink's own sources too; `bus_sources` reading the sink's per-port sources.
#[test]
fn connecting_and_disconnecting_a_bus_edge_round_trips() {
    let mut r = rig();
    let g = r.s.graph();
    let (ins, outs) = g.buses(r.stereo).expect("declared");
    assert!(ins.is_empty(), "no input channels, no input bus");
    assert_eq!(outs.len(), 1);
    assert_eq!(outs.get(0).unwrap().topology, named(2));
    assert_eq!(g.buses(NodeKey(u64::MAX)), None);

    g.add_source(r.sink, 0, GraphSource::Node(r.own, 0));
    assert!(all_are(&out0(&mut r.s), 0.125));

    let g = r.s.graph();
    g.connect_bus(BusSource::Node(bus(r.stereo)), bus(r.sink))
        .expect("stereo folds into mono");
    assert!(g.is_edited());
    assert_eq!(g.bus_sources(r.sink, 0), [BusSource::Node(bus(r.stereo))]);
    // A second connect of the same edge is the same edge.
    g.connect_bus(BusSource::Node(bus(r.stereo)), bus(r.sink))
        .unwrap();
    assert_eq!(g.bus_sources(r.sink, 0).len(), 1);
    // The fold (0.5 * 0.25 + 0.5 * 0.75) summed with the ordinary source
    // (bus edge's node sorts first): (0.125 + 0.375) + 0.125.
    let want = (0.5f32 * 0.25 + 0.5 * 0.75) + 0.125;
    assert_eq!(want, 0.625);
    assert!(all_are(&out0(&mut r.s), want));

    let g = r.s.graph();
    assert!(g.disconnect_bus(BusSource::Node(bus(r.stereo)), bus(r.sink)));
    assert!(!g.disconnect_bus(BusSource::Node(bus(r.stereo)), bus(r.sink)));
    assert!(g.is_edited());
    assert!(g.bus_sources(r.sink, 0).is_empty());
    assert_eq!(
        g.sources(r.sink, 0),
        [GraphSource::Node(r.own, 0)],
        "the author's own source stays"
    );
    assert!(all_are(&out0(&mut r.s), 0.125));
}

/// A refused bus edge changes nothing and marks nothing edited: the fold's
/// reason, a bus that is not there, a node that is not there.
///
/// Mutation (run): `connect_bus` marking the graph edited before the editor's
/// check.
#[test]
fn a_refused_bus_edge_changes_nothing() {
    let mut r = rig();
    let g = r.s.graph();
    let (three, _) = g.add(Unforkable(Src {
        values: vec![0.0; 3],
        out: discrete(3),
    }));
    r.s.update();
    let g = r.s.graph();
    let refused = g
        .connect_bus(BusSource::Node(bus(three)), bus(r.sink))
        .unwrap_err();
    assert_eq!(
        refused,
        BusError::Fold {
            from: BusSource::Node(bus(three)),
            to: bus(r.sink),
            error: FoldError::Discrete {
                src: layout(3),
                dst: layout(1)
            }
        }
    );
    assert!(matches!(
        g.connect_bus(
            BusSource::Node(BusRef {
                node: r.stereo,
                bus: 4
            }),
            bus(r.sink)
        ),
        Err(BusError::NoSuchBus { .. })
    ));
    assert!(matches!(
        g.connect_bus(BusSource::Node(bus(NodeKey(u64::MAX))), bus(r.sink)),
        Err(BusError::NoSuchNode { .. })
    ));
    assert!(!g.is_edited());
    assert!(g.bus_sources(r.sink, 0).is_empty());
}

/// The weights of a bus edge's lanes are the fold's: the session refuses an
/// edge gain on a lane by name, and removing a node takes its bus edges with
/// it so the next commit stands.
///
/// Mutations (run), each failing this test: `set_edge_gain` mapping the lane
/// refusal to `NoSuchEdge`; `Editor::remove` leaving the removed node among a
/// bus edge's sources (the edge is still listed).
#[test]
fn a_lane_takes_no_session_gain_and_a_removed_node_takes_its_bus_edges() {
    let mut r = rig();
    let g = r.s.graph();
    g.connect_bus(BusSource::Node(bus(r.stereo)), bus(r.sink))
        .unwrap();
    let lane = EdgeRef::port(r.sink, 0, GraphSource::Node(r.stereo, 0));
    assert_eq!(
        g.set_edge_gain(lane, Amplitude::new(0.5)),
        Err(EdgeGainError::BusLane(lane))
    );
    assert!(all_are(&out0(&mut r.s), 0.5));

    let g = r.s.graph();
    assert!(g.remove(r.stereo));
    assert!(g.bus_sources(r.sink, 0).is_empty());
    assert!(all_are(&out0(&mut r.s), 0.0));
}
