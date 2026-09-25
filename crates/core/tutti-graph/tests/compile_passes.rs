//! The compiler's passes, one claim at a time: order, cycles, the latency
//! solve against `tutti_types::latency`, colouring, coarsening, placement.
//!
//! The 13 shapes are `tutti-core/tests/topology_compile.rs`'s, ported: the
//! same graphs, now compiled to a `Plan` instead of a `Net`, so the two
//! compilers answer the same questions about the same values.

mod common;

use std::collections::BTreeMap;

use common::{bits, input_signal, Kind, Pair, SpecBehaviour};
use tutti_graph::{
    compile, verify, CompileError, CycleEdge, DelayKey, EventEdge, EventIn, EventOut, GraphInvalid,
    GraphSpec, Op, PortKind, Shape, Shapes,
};
use tutti_types::graph::{Edge, FeedbackFrom, InPort, Invalid, NodeSpec, OutPort, Source};
use tutti_types::latency::{self, DelayInsertion, LatencyGraph};
use tutti_types::{ChannelLayout, Latency, NodeKey, Samples, Tail, Topology};

/// The `MaxBlock` every plan in this file is compiled for.
const PREP: usize = 128;

const A: NodeKey = NodeKey(10);
const B: NodeKey = NodeKey(20);
const C: NodeKey = NodeKey(30);
const D: NodeKey = NodeKey(40);

fn spec(kind: &str, ins: u16, outs: u16) -> NodeSpec {
    NodeSpec::new(
        kind,
        ChannelLayout::from_count(ins),
        ChannelLayout::from_count(outs),
    )
    .with_tail(Tail::None)
}

fn at(node: NodeKey, port: u16) -> InPort {
    InPort { node, port }
}

fn out(node: NodeKey, port: u16) -> OutPort {
    OutPort { node, port }
}

fn edge(t: &mut Topology, sink: InPort, from: OutPort) {
    t.edges.insert(sink, Edge::Direct(Source::Node(from)));
}

/// `topology_compile.rs`'s thirteen shapes, verbatim.
fn shapes() -> Vec<(&'static str, Topology)> {
    let mut all = Vec::new();

    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes.insert(B, spec("gain", 1, 1));
    edge(&mut t, at(B, 0), out(A, 0));
    t.outputs = vec![Source::Node(out(B, 0))];
    all.push(("chain", t));

    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes.insert(B, spec("gain", 1, 1));
    t.nodes.insert(C, spec("gain", 1, 1));
    edge(&mut t, at(B, 0), out(A, 0));
    edge(&mut t, at(C, 0), out(A, 0));
    t.outputs = vec![Source::Node(out(B, 0)), Source::Node(out(C, 0))];
    all.push(("fan_out", t));

    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes.insert(B, spec("dc", 0, 1));
    t.nodes.insert(C, spec("sum", 2, 1));
    edge(&mut t, at(C, 0), out(A, 0));
    edge(&mut t, at(C, 1), out(B, 0));
    t.outputs = vec![Source::Node(out(C, 0))];
    all.push(("mix_fan_in", t));

    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes.insert(B, spec("gain", 1, 1));
    t.nodes.insert(C, spec("gain", 1, 1));
    t.nodes.insert(D, spec("sum", 2, 1));
    edge(&mut t, at(B, 0), out(A, 0));
    edge(&mut t, at(C, 0), out(A, 0));
    edge(&mut t, at(D, 0), out(B, 0));
    edge(&mut t, at(D, 1), out(C, 0));
    t.outputs = vec![Source::Node(out(D, 0))];
    all.push(("diamond", t));

    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes
        .insert(B, spec("gain", 1, 1).with_latency(Samples(512)));
    t.nodes.insert(C, spec("gain", 1, 1));
    t.nodes.insert(D, spec("sum", 2, 1));
    edge(&mut t, at(B, 0), out(A, 0));
    edge(&mut t, at(C, 0), out(A, 0));
    edge(&mut t, at(D, 0), out(B, 0));
    edge(&mut t, at(D, 1), out(C, 0));
    t.outputs = vec![Source::Node(out(D, 0))];
    all.push(("pdc_diamond", t));

    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes
        .insert(B, spec("gain", 1, 1).with_latency(Samples(128)));
    t.nodes
        .insert(C, spec("gain", 1, 1).with_latency(Samples(64)));
    edge(&mut t, at(B, 0), out(A, 0));
    edge(&mut t, at(C, 0), out(B, 0));
    t.outputs = vec![Source::Node(out(C, 0))];
    all.push(("serial_latency", t));

    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 2));
    t.outputs = vec![Source::Node(out(A, 0)), Source::Node(out(A, 1))];
    all.push(("multi_output", t));

    let mut t = Topology {
        inputs: ChannelLayout::STEREO,
        ..Topology::default()
    };
    t.nodes.insert(A, spec("fan", 2, 2));
    t.edges.insert(at(A, 0), Edge::Direct(Source::Global(0)));
    t.edges.insert(at(A, 1), Edge::Direct(Source::Global(1)));
    t.outputs = vec![Source::Node(out(A, 0)), Source::Node(out(A, 1))];
    all.push(("global_input", t));

    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes.insert(B, spec("sum", 2, 1));
    edge(&mut t, at(B, 0), out(A, 0));
    t.edges.insert(at(B, 1), Edge::Direct(Source::Zero));
    t.outputs = vec![Source::Node(out(B, 0))];
    all.push(("zero_sourced_port", t));

    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.outputs = vec![Source::Node(out(A, 0)), Source::Zero];
    all.push(("silent_output_channel", t));

    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 6));
    t.nodes.insert(B, spec("fan", 6, 6));
    for port in 0..6 {
        edge(&mut t, at(B, port), out(A, port));
    }
    t.outputs = (0..6).map(|p| Source::Node(out(B, p))).collect();
    all.push(("six_channel", t));

    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes.insert(
        B,
        spec("gain", 1, 1).with_tail(Tail::Finite(Samples(2_048))),
    );
    t.nodes
        .insert(C, spec("gain", 1, 1).with_tail(Tail::Finite(Samples(256))));
    edge(&mut t, at(B, 0), out(A, 0));
    edge(&mut t, at(C, 0), out(B, 0));
    t.outputs = vec![Source::Node(out(C, 0))];
    all.push(("tail_chain", t));

    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes
        .insert(B, spec("gain", 1, 1).with_latency(Samples(999)));
    edge(&mut t, at(B, 0), out(A, 0));
    t.outputs = vec![Source::Node(out(A, 0))];
    all.push(("orphan_branch", t));

    all
}

/// A `Spec` test node per spec — its declared latency and tail, its kind's
/// behaviour.
fn kinds_for(t: &Topology) -> BTreeMap<NodeKey, Kind> {
    t.nodes
        .iter()
        .map(|(&k, s)| {
            let behaviour = match s.kind.as_str() {
                "dc" => SpecBehaviour::Dc(s.scalar("value").unwrap_or(0.5)),
                "gain" => SpecBehaviour::Gain(s.scalar("gain").unwrap_or(0.75)),
                "sum" => SpecBehaviour::Sum,
                _ => SpecBehaviour::Fan,
            };
            (
                k,
                Kind::Spec {
                    behaviour,
                    ins: s.inputs.count() as usize,
                    outs: s.outputs.count() as usize,
                    latency: s.latency.get(),
                    tail: s.tail,
                },
            )
        })
        .collect()
}

/// The shapes a spec implies, without building any node.
fn shapes_of_spec(t: &Topology) -> Shapes {
    t.nodes
        .iter()
        .map(|(&k, s)| {
            (
                k,
                Shape::audio(s.inputs, s.outputs)
                    .with_latency(Latency::new(s.latency))
                    .with_tail(s.tail),
            )
        })
        .collect()
}

fn compiled(t: &Topology) -> tutti_graph::Plan {
    let valid = GraphSpec::new(t.clone()).validate().expect("valid");
    compile(&valid, &shapes_of_spec(t), &common::prepare(PREP), None)
        .expect("compiles")
        .0
}

// ---------------------------------------------------------------------------
// Order.
// ---------------------------------------------------------------------------

/// With no event edges, the compiler's one order *is* `Topology::topo_order`.
///
/// Mutation: in `order::kahn`, seed the ready stack without the `.rev()` →
/// the largest ready key pops first → `mix_fan_in`'s two roots run in the
/// wrong order → fails.
#[test]
fn the_order_is_topology_topo_order() {
    assert_eq!(shapes().len(), 13, "the shape list is the coverage");
    for (name, t) in shapes() {
        assert_eq!(
            compiled(&t).order(),
            t.topo_order().expect("acyclic").as_slice(),
            "{name}"
        );
    }
}

/// Ties break toward the smaller key even when the dependents arrive through
/// different edge kinds. `X` feeds `B` by audio and `A` by events; both
/// become ready together, and `A < B` must run first.
///
/// Mutation: in `order::kahn`, build `dependents` by visiting sinks in
/// reverse (`preds.iter().enumerate().rev()`) → `B` pops before `A` → fails
/// (in debug via the sortedness assert, in release on the order).
#[test]
fn ties_break_by_key_across_edge_kinds() {
    let x = NodeKey(1);
    let (a, b) = (NodeKey(5), NodeKey(9));
    let mut t = Topology::default();
    t.nodes.insert(x, spec("src", 0, 1));
    t.nodes.insert(a, spec("ev", 0, 0));
    t.nodes.insert(b, spec("gain", 1, 1));
    edge(&mut t, at(b, 0), out(x, 0));
    let mut g = GraphSpec::new(t);
    g.connect_events(
        EventIn { node: a, port: 0 },
        EventEdge::Direct(EventOut { node: x, port: 0 }),
    );
    let shapes: Shapes = [
        (
            x,
            Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_events(0, 1),
        ),
        (
            a,
            Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(1, 0),
        ),
        (b, Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO)),
    ]
    .into();
    let plan = compile(
        &g.validate().unwrap(),
        &shapes,
        &common::prepare(PREP),
        None,
    )
    .unwrap()
    .0;
    assert_eq!(plan.order(), &[x, a, b]);
}

// ---------------------------------------------------------------------------
// Latency: the plan's delays are exactly `latency::compensate`'s.
// ---------------------------------------------------------------------------

/// A `DelayInsertion` that records what `compensate` asks for, over a
/// `Topology` it delegates to.
struct Recorder<'a> {
    t: &'a Topology,
    inputs: Vec<(NodeKey, usize, Samples)>,
    outputs: Vec<(usize, Samples)>,
}

impl LatencyGraph for Recorder<'_> {
    type Node = NodeKey;
    fn nodes(&self) -> impl Iterator<Item = NodeKey> {
        LatencyGraph::nodes(self.t)
    }
    fn latency(&self, n: NodeKey) -> Samples {
        LatencyGraph::latency(self.t, n)
    }
    fn inputs(&self, n: NodeKey) -> impl Iterator<Item = Option<NodeKey>> {
        LatencyGraph::inputs(self.t, n)
    }
    fn outputs(&self) -> impl Iterator<Item = Option<NodeKey>> {
        LatencyGraph::outputs(self.t)
    }
}

impl DelayInsertion for Recorder<'_> {
    fn clear_delays(&mut self) {
        self.inputs.clear();
        self.outputs.clear();
    }
    fn delay_input(&mut self, node: NodeKey, port: usize, by: Samples) {
        self.inputs.push((node, port, by));
    }
    fn delay_output(&mut self, channel: usize, by: Samples) {
        self.outputs.push((channel, by));
    }
}

/// Every per-port delay, every output alignment, the per-channel pre-roll and
/// the total agree with `tutti_types::latency` on all thirteen shapes.
///
/// Mutation: in `compile`, use `arrival[n]` (not `arrival + latency`) as a
/// source's departure → `pdc_diamond` loses its 512-frame delay → fails.
/// Mutation: emit output rings for `Source::Zero` channels and compare the
/// ring list → `silent_output_channel` still passes (the recorder lists the
/// zero channel too), which is why zero channels are compared through
/// `compensation()` and excluded from the ring comparison explicitly.
#[test]
fn pdc_delays_equal_latency_compensate() {
    let mut nonzero = 0;
    for (name, t) in shapes() {
        let plan = compiled(&t);
        let mut rec = Recorder {
            t: &t,
            inputs: Vec::new(),
            outputs: Vec::new(),
        };
        let comp = latency::compensate(&mut rec);
        assert_eq!(comp, latency::plan(&t), "{name}: plan and compensate agree");

        let mut want_in: Vec<(DelayKey, Samples)> = rec
            .inputs
            .iter()
            .map(|&(n, p, by)| {
                let sink = at(n, p as u16);
                let Edge::Direct(from) = t.edges[&sink] else {
                    unreachable!("latency::plan delays only direct edges")
                };
                (DelayKey::Audio { at: sink, from }, by)
            })
            .collect();
        want_in.sort();
        let mut got_in: Vec<(DelayKey, Samples)> = plan
            .delays()
            .iter()
            .filter(|d| matches!(d.key, DelayKey::Audio { .. }))
            .map(|d| (d.key, d.len))
            .collect();
        got_in.sort();
        assert_eq!(got_in, want_in, "{name}: input delays");

        let want_out: Vec<(DelayKey, Samples)> = rec
            .outputs
            .iter()
            .filter(|&&(ch, _)| t.outputs[ch] != Source::Zero)
            .map(|&(ch, by)| {
                let key = DelayKey::Output {
                    channel: ch as u16,
                    from: t.outputs[ch],
                };
                (key, by)
            })
            .collect();
        let got_out: Vec<(DelayKey, Samples)> = plan
            .delays()
            .iter()
            .filter(|d| matches!(d.key, DelayKey::Output { .. }))
            .map(|d| (d.key, d.len))
            .collect();
        assert_eq!(got_out, want_out, "{name}: output alignment");

        assert_eq!(
            plan.total_latency().samples(),
            comp.total(),
            "{name}: total"
        );
        for ch in 0..t.outputs.len() {
            assert_eq!(
                plan.compensation()[ch],
                comp.for_channel(ch),
                "{name}: pre-roll of channel {ch}"
            );
        }
        nonzero += usize::from(!comp.is_empty());
    }
    assert!(nonzero >= 2, "the agreement is not vacuous");
}

/// The rings the plan inserts actually align the paths: rendered through the
/// executor, the thirteen shapes match the reference interpreter sample for
/// sample, at a block size that makes the 512-frame ring span blocks.
///
/// Mutation: in `Executor::process`, skip `Op::Delay` (leave `dst` as it was)
/// → `pdc_diamond` diverges → fails.
#[test]
fn the_ported_shapes_render_like_the_reference() {
    for (name, t) in shapes() {
        // `global_input` reads two graph inputs and the harness feeds one;
        // global inputs are covered by the differential suite instead.
        if t.inputs.count() > 1 {
            continue;
        }
        let kinds = kinds_for(&t);
        let valid = GraphSpec::new(t.clone()).validate().expect("valid");
        let mut pair = Pair::new(128);
        pair.switch(&valid, &kinds);
        let mut frame = 0u64;
        for _ in 0..12 {
            let n = 100;
            let input: Vec<f32> = input_signal(frame, n);
            let (a, b) = pair.block(n, &input);
            assert_eq!(bits(&a), bits(&b), "{name} at frame {frame}");
            frame += n as u64;
        }
    }
}

// ---------------------------------------------------------------------------
// Cycles.
// ---------------------------------------------------------------------------

fn gain_kinds(keys: &[NodeKey]) -> Shapes {
    keys.iter()
        .map(|&k| {
            (
                k,
                Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO).with_events(1, 1),
            )
        })
        .collect()
}

fn two_node_spec() -> GraphSpec {
    let mut t = Topology::default();
    t.nodes.insert(A, spec("n", 1, 1));
    t.nodes.insert(B, spec("n", 1, 1));
    GraphSpec::new(t)
}

/// A purely audio cycle is the value's own error, reported by
/// `Topology::validate` exactly as before.
///
/// Mutation: in `GraphSpec::validate`, ignore the topology's errors → the
/// spec validates → fails.
#[test]
fn an_audio_cycle_is_invalid() {
    let mut g = two_node_spec();
    edge(&mut g.topology, at(B, 0), out(A, 0));
    edge(&mut g.topology, at(A, 0), out(B, 0));
    let errs = g.validate().expect_err("cyclic");
    assert!(errs
        .iter()
        .any(|e| matches!(e, GraphInvalid::Topology(Invalid::Cycle { .. }))));
}

/// A cycle that runs through an event edge is only visible to the compiler,
/// and its error names every edge in the cycle — by port.
///
/// Mutation: in `compile`, filter the SCC edges with `s == d` only (self-loops)
/// → the two-node cycle is missed → compiles → fails.
#[test]
fn a_cycle_through_an_event_edge_is_an_error_naming_its_edges() {
    let mut g = two_node_spec();
    edge(&mut g.topology, at(B, 0), out(A, 0));
    g.connect_events(
        EventIn { node: A, port: 0 },
        EventEdge::Direct(EventOut { node: B, port: 0 }),
    );
    let valid = g.validate().expect("the value alone cannot see it");
    let err =
        compile(&valid, &gain_kinds(&[A, B]), &common::prepare(PREP), None).expect_err("cyclic");
    assert_eq!(
        err,
        CompileError::Cycle {
            edges: vec![
                CycleEdge::Audio(at(B, 0)),
                CycleEdge::Event {
                    at: EventIn { node: A, port: 0 },
                    from: EventOut { node: B, port: 0 }
                },
            ]
        }
    );
}

/// An event edge from a node to itself is a cycle too.
///
/// Mutation: drop the `s == d` arm of the SCC filter → a one-node component
/// is never "cyclic" → compiles → fails.
#[test]
fn an_event_self_loop_is_a_cycle() {
    let mut g = two_node_spec();
    g.connect_events(
        EventIn { node: A, port: 0 },
        EventEdge::Direct(EventOut { node: A, port: 0 }),
    );
    let valid = g.validate().expect("valid value");
    assert!(matches!(
        compile(&valid, &gain_kinds(&[A, B]), &common::prepare(PREP), None),
        Err(CompileError::Cycle { .. })
    ));
}

/// Feedback edges — audio and event — break those same cycles, and become
/// captures read next block.
///
/// Mutation: treat `EventEdge::Feedback` as a direct dependency in the SCC
/// pass → the event cycle is reported → fails.
#[test]
fn feedback_edges_break_cycles() {
    let mut g = two_node_spec();
    edge(&mut g.topology, at(B, 0), out(A, 0));
    g.topology.edges.insert(
        at(A, 0),
        Edge::Feedback(FeedbackFrom::one_block(out(B, 0), Samples(PREP))),
    );
    g.connect_events(
        EventIn { node: A, port: 0 },
        EventEdge::feedback(EventOut { node: B, port: 0 }, Samples(PREP)),
    );
    let valid = g.validate().expect("feedback breaks the cycle");
    let (plan, _) =
        compile(&valid, &gain_kinds(&[A, B]), &common::prepare(PREP), None).expect("compiles");
    verify(&plan).expect("sound");
    assert_eq!(plan.feedback(PortKind::Audio).len(), 1);
    assert_eq!(plan.feedback(PortKind::Event).len(), 1);
    let captures = plan
        .ops()
        .iter()
        .filter(|op| matches!(op, Op::Capture { .. } | Op::EventCapture { .. }))
        .count();
    assert_eq!(captures, 2);
}

// ---------------------------------------------------------------------------
// Shape checks.
// ---------------------------------------------------------------------------

/// A unit whose shape disagrees with its spec, a node with no shape, an event
/// port past a node's declared ports, and an over-wide node are all compile
/// errors naming the node.
///
/// Mutation: delete the width comparison in `compile` → the first case
/// compiles → fails.
#[test]
fn shape_disagreements_are_errors() {
    let g = two_node_spec();
    let valid = g.validate().unwrap();
    let mut shapes = gain_kinds(&[A, B]);
    shapes.insert(A, Shape::audio(ChannelLayout::STEREO, ChannelLayout::MONO));
    assert!(matches!(
        compile(&valid, &shapes, &common::prepare(PREP), None),
        Err(CompileError::WidthMismatch { node: A, .. })
    ));

    let shapes = gain_kinds(&[A]);
    assert_eq!(
        compile(&valid, &shapes, &common::prepare(PREP), None).unwrap_err(),
        CompileError::MissingShape { node: B }
    );

    let mut g = two_node_spec();
    g.connect_events(
        EventIn { node: A, port: 3 },
        EventEdge::Direct(EventOut { node: B, port: 0 }),
    );
    let valid = g.validate().unwrap();
    assert_eq!(
        compile(&valid, &gain_kinds(&[A, B]), &common::prepare(PREP), None).unwrap_err(),
        CompileError::EventPortOutOfRange {
            at: EventIn { node: A, port: 3 },
            from: None
        }
    );

    let mut t = Topology::default();
    t.nodes.insert(A, spec("wide", 0, 65));
    let valid = GraphSpec::new(t).validate().unwrap();
    let shapes: Shapes = [(
        A,
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::from_count(65)),
    )]
    .into();
    assert_eq!(
        compile(&valid, &shapes, &common::prepare(PREP), None).unwrap_err(),
        CompileError::TooManyPorts { node: A, count: 65 }
    );
}

// ---------------------------------------------------------------------------
// Colouring.
// ---------------------------------------------------------------------------

/// A long serial chain reuses slots — it does not need one per edge.
///
/// Mutation: make `colour` always open a new slot → 40 slots → fails.
#[test]
fn a_chain_reuses_slots() {
    let mut t = Topology::default();
    let keys: Vec<NodeKey> = (0..40).map(NodeKey).collect();
    t.nodes.insert(keys[0], spec("dc", 0, 1));
    for w in keys.windows(2) {
        t.nodes.insert(w[1], spec("gain", 1, 1));
        edge(&mut t, at(w[1], 0), out(w[0], 0));
    }
    t.outputs = vec![Source::Node(out(keys[39], 0))];
    let plan = compiled(&t);
    verify(&plan).expect("sound");
    // Zero slot + at most two alternating coloured slots.
    assert!(
        plan.slots(PortKind::Audio) <= 3,
        "{} slots for a chain",
        plan.slots(PortKind::Audio)
    );
}

/// The case serial liveness gets wrong. In serial order `[A, B, C]`, `A`'s
/// value is dead after `B`, so a serial allocator hands its slot to `C`. But
/// `C` does not depend on `A` or `B` — a parallel executor may run `C`
/// alongside `B`, and `C` would overwrite what `B` is reading. The partial
/// order forbids the share.
///
/// Mutation: in `colour::finished_before`, compare serial op positions
/// (`r < w`) instead of reachability → `A` and `C` share → fails.
#[test]
fn colouring_respects_the_partial_order_not_the_serial_one() {
    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes.insert(B, spec("gain", 1, 1));
    t.nodes.insert(C, spec("dc", 0, 1));
    edge(&mut t, at(B, 0), out(A, 0));
    t.outputs = vec![Source::Node(out(B, 0)), Source::Node(out(C, 0))];
    let plan = compiled(&t);
    assert_eq!(
        plan.order(),
        &[A, B, C],
        "the serial order the test assumes"
    );
    let slot_of = |key: NodeKey| {
        let unit = plan.units().iter().position(|u| u.key == key).unwrap() as u32;
        plan.ops()
            .iter()
            .find_map(|op| match *op {
                Op::Node {
                    unit: u, audio_out, ..
                } if u == unit => Some(plan.audio_list()[audio_out.start as usize]),
                _ => None,
            })
            .unwrap()
    };
    assert_ne!(slot_of(A), slot_of(C), "C may run beside B, which reads A");
    verify(&plan).expect("sound");
}

fn in_place_spec(ports_read_twice: bool) -> (GraphSpec, Shapes) {
    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes.insert(B, spec("gain", 2, 2));
    edge(&mut t, at(B, 0), out(A, 0));
    if ports_read_twice {
        edge(&mut t, at(B, 1), out(A, 0));
    }
    t.outputs = vec![Source::Node(out(B, 0)), Source::Node(out(B, 1))];
    let shapes: Shapes = [
        (A, Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)),
        (
            B,
            Shape::audio(ChannelLayout::STEREO, ChannelLayout::STEREO).with_in_place(),
        ),
    ]
    .into();
    (GraphSpec::new(t), shapes)
}

/// In-place aliasing happens when it is legal: the node opted in, the input's
/// last reader is this node, and it reads the value on one port only.
///
/// Mutation: drop the "read on exactly one port" check in `compile` → the
/// second case aliases port 0, and port 1 would read the node's own output →
/// the verifier panics inside `compile` (debug) → fails.
#[test]
fn in_place_aliasing_happens_exactly_when_legal() {
    let (g, shapes) = in_place_spec(false);
    let plan = compile(
        &g.validate().unwrap(),
        &shapes,
        &common::prepare(PREP),
        None,
    )
    .unwrap()
    .0;
    assert!(plan.in_place(B).get(0), "sole reader, one port: aliased");

    let (g, shapes) = in_place_spec(true);
    let plan = compile(
        &g.validate().unwrap(),
        &shapes,
        &common::prepare(PREP),
        None,
    )
    .unwrap()
    .0;
    assert_eq!(plan.in_place(B).0, 0, "read on two ports: not aliased");

    // Not opted in: never aliased.
    let (g, mut shapes) = in_place_spec(false);
    shapes.insert(
        B,
        Shape::audio(ChannelLayout::STEREO, ChannelLayout::STEREO),
    );
    let plan = compile(
        &g.validate().unwrap(),
        &shapes,
        &common::prepare(PREP),
        None,
    )
    .unwrap()
    .0;
    assert_eq!(plan.in_place(B).0, 0);

    // Another reader that may run concurrently: not aliased.
    let (mut g, shapes) = in_place_spec(false);
    let mut shapes = shapes;
    g.topology.nodes.insert(C, spec("gain", 1, 1));
    edge(&mut g.topology, at(C, 0), out(A, 0));
    g.topology.outputs.push(Source::Node(out(C, 0)));
    shapes.insert(C, Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO));
    let plan = compile(
        &g.validate().unwrap(),
        &shapes,
        &common::prepare(PREP),
        None,
    )
    .unwrap()
    .0;
    assert_eq!(plan.in_place(B).0, 0, "C still reads A's slot");
}

// ---------------------------------------------------------------------------
// Coarsening and placement.
// ---------------------------------------------------------------------------

/// A pure chain coarsens to one task; a diamond does not.
///
/// Mutation: in `coarsen`, never fuse → the chain has 40+ tasks → fails.
#[test]
fn chains_coarsen_into_one_task() {
    let mut t = Topology::default();
    let keys: Vec<NodeKey> = (0..10).map(NodeKey).collect();
    t.nodes.insert(keys[0], spec("dc", 0, 1));
    for w in keys.windows(2) {
        t.nodes.insert(w[1], spec("gain", 1, 1));
        edge(&mut t, at(w[1], 0), out(w[0], 0));
    }
    t.outputs = vec![Source::Node(out(keys[9], 0))];
    let plan = compiled(&t);
    assert_eq!(plan.tasks().len(), 1, "10 nodes + output op, one chain");
    assert_eq!(plan.task_activation(), &[0]);

    let diamond = shapes()
        .into_iter()
        .find(|(n, _)| *n == "diamond")
        .unwrap()
        .1;
    let plan = compiled(&diamond);
    assert!(plan.tasks().len() > 1);
    // Every op is in exactly one task.
    let mut seen: Vec<u32> = plan.task_ops().to_vec();
    seen.sort();
    assert_eq!(seen, (0..plan.ops().len() as u32).collect::<Vec<_>>());
}

/// Placement diffs keys, not shapes: a surviving key keeps its index, a
/// removed one retires, a regenerated one is replaced in place, and a new one
/// takes the lowest free index.
///
/// Mutation: in `compile`'s placement, ignore `prev` → every key is an
/// insert → fails.
#[test]
fn the_delta_moves_only_what_changed() {
    let mut t = Topology::default();
    for k in [A, B, C] {
        t.nodes.insert(k, spec("dc", 0, 1));
    }
    let shapes = shapes_of_spec(&t);
    let (first, delta) = compile(
        &GraphSpec::new(t.clone()).validate().unwrap(),
        &shapes,
        &common::prepare(PREP),
        None,
    )
    .unwrap();
    assert_eq!(delta.insert.len(), 3);
    let idx = |p: &tutti_graph::Plan, k| p.unit(k).unwrap().idx;

    let mut g = GraphSpec::new(t);
    g.topology.nodes.remove(&B);
    g.topology.nodes.insert(D, spec("dc", 0, 1));
    g.generations.insert(C, 1);
    let mut shapes = shapes_of_spec(&g.topology);
    shapes.remove(&B);
    let (second, delta) = compile(
        &g.validate().unwrap(),
        &shapes,
        &common::prepare(PREP),
        Some(&first),
    )
    .unwrap();

    assert_eq!(idx(&second, A), idx(&first, A), "A kept its place");
    assert_eq!(delta.retire.len(), 1);
    assert_eq!(delta.retire[0].key, B);
    assert_eq!(delta.replace.len(), 1);
    assert_eq!(delta.replace[0].0.key, C);
    assert_eq!(delta.replace[0].1.gen, 1);
    assert_eq!(idx(&second, C), idx(&first, C), "replaced in place");
    assert_eq!(delta.insert.len(), 1);
    assert_eq!(delta.insert[0].key, D);
    assert_eq!(idx(&second, D), idx(&first, B), "D reuses B's freed index");
    assert_eq!(delta.store_len, 3);
}

/// Decision (review): a `Source::Global` input that merges with a latent
/// path **is** delayed to align, like any other merge-point source. This is
/// where the compiler deliberately differs from `latency::plan`, which
/// treats a global input as outside the graph (unified in doc 013 Phase 5).
///
/// Mutation: in `compile`, give `Source::Global` ports no delay (the old
/// rule) → the plan has no `DelayKey::Audio { from: Global(0) }` → fails.
/// The reference computes the same delay independently.
#[test]
fn a_global_input_merging_with_a_latent_path_is_delayed() {
    let mut t = Topology {
        inputs: ChannelLayout::MONO,
        ..Topology::default()
    };
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes
        .insert(B, spec("gain", 1, 1).with_latency(Samples(48)));
    t.nodes.insert(C, spec("sum", 2, 1));
    edge(&mut t, at(B, 0), out(A, 0));
    edge(&mut t, at(C, 0), out(B, 0));
    t.edges.insert(at(C, 1), Edge::Direct(Source::Global(0)));
    t.outputs = vec![Source::Node(out(C, 0))];

    let plan = compiled(&t);
    let key = DelayKey::Audio {
        at: at(C, 1),
        from: Source::Global(0),
    };
    assert_eq!(plan.delay(key), Samples(48));
    // `latency::plan` does not: the two solves differ here until Phase 5.
    let mut rec = Recorder {
        t: &t,
        inputs: Vec::new(),
        outputs: Vec::new(),
    };
    latency::compensate(&mut rec);
    assert!(
        rec.inputs.is_empty(),
        "latency::plan never delays a global input"
    );

    let mut pair = Pair::new(128);
    pair.switch(
        &GraphSpec::new(t.clone()).validate().unwrap(),
        &kinds_for(&t),
    );
    assert_eq!(pair.reference.delay(key), Samples(48));
    let mut frame = 0;
    for _ in 0..6 {
        let (a, b) = pair.block(100, &input_signal(frame, 100));
        assert_eq!(bits(&a), bits(&b));
        frame += 100;
    }
}

/// N3: a shape whose latency or tail disagrees with its spec is refused, as
/// a width mismatch is — one of them is stale, and compiling either would
/// make `latency::plan` over the value disagree with the plan.
///
/// Mutation: delete the latency comparison in `compile` → compiles → fails;
/// likewise the tail comparison.
#[test]
fn a_shape_disagreeing_on_latency_or_tail_is_refused() {
    let mut t = Topology::default();
    t.nodes
        .insert(A, spec("gain", 1, 1).with_latency(Samples(10)));
    let valid = GraphSpec::new(t).validate().unwrap();
    let late: Shapes = [(
        A,
        Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO)
            .with_latency(Latency::new(Samples(12))),
    )]
    .into();
    assert!(matches!(
        compile(&valid, &late, &common::prepare(PREP), None),
        Err(CompileError::LatencyMismatch { node: A, .. })
    ));
    let tailed: Shapes = [(
        A,
        Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO)
            .with_latency(Latency::new(Samples(10)))
            .with_tail(Tail::Unbounded),
    )]
    .into();
    assert!(matches!(
        compile(&valid, &tailed, &common::prepare(PREP), None),
        Err(CompileError::TailMismatch { node: A, .. })
    ));
}

// ---------------------------------------------------------------------------
// Event resolution (doc 013 §6 item 5).
// ---------------------------------------------------------------------------

/// An automation source `A` feeding sink `B`'s event input, with `B`
/// declaring `sink` resolution; the edge marked `required` when given.
fn automation_into(
    sink: tutti_graph::Resolution,
    required: Option<tutti_graph::Resolution>,
) -> Result<(), CompileError> {
    let mut g = two_node_spec();
    let (lane, target) = (EventOut { node: A, port: 0 }, EventIn { node: B, port: 0 });
    g.connect_events(target, EventEdge::Direct(lane));
    if let Some(r) = required {
        g.require_resolution(target, lane, r);
    }
    let mono = || Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO);
    let shapes: Shapes = [
        (A, mono().with_events(0, 1)),
        (B, mono().with_events(1, 0).with_event_resolution(sink)),
    ]
    .into();
    let valid = g.validate().expect("valid");
    compile(&valid, &shapes, &common::prepare(PREP), None).map(|_| ())
}

/// The rule: a marked edge is refused, by name, when its sink declares a
/// coarser resolution; an unmarked edge never is; a fine enough sink always
/// compiles.
///
/// Mutation: drop the resolution check in `compile` → the block-rate sink
/// compiles → fails. Mutation: make `Resolution::honours` compare
/// `Frames(n)` against `Frames(m)` the wrong way (`n >= m`) → the
/// 8-frame sink refuses the 16-frame requirement → fails.
#[test]
fn a_sample_accurate_edge_into_a_block_node_is_refused() {
    use tutti_graph::Resolution::{Block, Frames, Sample};
    assert_eq!(
        automation_into(Block, Some(Sample)),
        Err(CompileError::ResolutionTooCoarse {
            at: EventIn { node: B, port: 0 },
            from: EventOut { node: A, port: 0 },
            required: Sample,
            sink: Block,
        })
    );
    assert_eq!(automation_into(Block, None), Ok(()), "unmarked: allowed");
    assert_eq!(automation_into(Sample, Some(Sample)), Ok(()));
    assert_eq!(automation_into(Frames(8), Some(Frames(16))), Ok(()));
    assert!(automation_into(Frames(8), Some(Sample)).is_err());
    assert!(automation_into(Frames(8), Some(Frames(4))).is_err());
    assert_eq!(automation_into(Block, Some(Block)), Ok(()));
}

/// A mark with no edge under it is a stale mark, and invalid: it would
/// otherwise pass unchecked forever.
///
/// Mutation: skip the `RequirementWithoutEdge` check in `validate` → the
/// spec validates → fails.
#[test]
fn a_resolution_mark_without_its_edge_is_invalid() {
    let mut g = two_node_spec();
    let (from, at) = (EventOut { node: A, port: 0 }, EventIn { node: B, port: 0 });
    g.require_resolution(at, from, tutti_graph::Resolution::Sample);
    assert_eq!(
        g.validate().err(),
        Some(vec![GraphInvalid::RequirementWithoutEdge { at, from }])
    );
}

/// New nodes promise sample accuracy by default; the `Legacy` adapter,
/// whose units receive no events at all, declares `Block`.
///
/// Mutation: drop `.with_event_resolution(Resolution::Block)` from
/// `Legacy::probe` → fails.
#[test]
fn nodes_default_to_sample_and_legacy_declares_block() {
    use tutti_graph::{Legacy, Resolution};
    let s = Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO);
    assert_eq!(s.event_resolution, Resolution::Sample);
    let legacy = Legacy::new(fundsp::prelude32::pass());
    assert_eq!(legacy.shape().event_resolution, Resolution::Block);
}

/// Removing a node removes the resolution marks on its edges with it, so a
/// removal never leaves a stale mark that fails the next commit.
///
/// Mutation: drop the `required_resolution.retain` in `Editor::remove` →
/// the commit after the removal is `Invalid(RequirementWithoutEdge)` →
/// fails.
#[test]
fn removing_a_node_removes_its_resolution_marks() {
    let (mut ed, _exec) = tutti_graph::Editor::new(common::prepare(PREP));
    ed.insert(
        A,
        "emit",
        common::TestNode::new(Kind::Emitter {
            period: 4,
            phase: 0,
        }),
    );
    ed.insert(
        B,
        "consume",
        common::TestNode::new(Kind::Consumer { inputs: 1 }),
    );
    let (from, at) = (EventOut { node: A, port: 0 }, EventIn { node: B, port: 0 });
    ed.spec_mut().connect_events(at, EventEdge::Direct(from));
    ed.spec_mut()
        .require_resolution(at, from, tutti_graph::Resolution::Sample);
    ed.commit().expect("the consumer honours samples");
    ed.remove(A);
    assert_eq!(ed.commit(), Ok(()));
    assert!(ed.spec().required_resolution.is_empty());
}

/// Disconnecting a marked edge drops its mark with it, so the disconnect
/// cannot wedge every later commit with `RequirementWithoutEdge`; a mark can
/// also be dropped alone.
///
/// Mutation: in `GraphSpec::disconnect_events`, keep the mark → the next
/// commit is `Invalid(RequirementWithoutEdge)` → fails. Mutation: make
/// `unrequire_resolution` a no-op → the block-rate sink still refuses the
/// edge → fails.
#[test]
fn disconnecting_an_edge_drops_its_resolution_mark() {
    let (mut ed, _exec) = tutti_graph::Editor::new(common::prepare(PREP));
    ed.insert(
        A,
        "emit",
        common::TestNode::new(Kind::Emitter {
            period: 4,
            phase: 0,
        }),
    );
    ed.insert(
        B,
        "consume",
        common::TestNode::new(Kind::Consumer { inputs: 1 }),
    );
    let (from, at) = (EventOut { node: A, port: 0 }, EventIn { node: B, port: 0 });
    ed.spec_mut().connect_events(at, EventEdge::Direct(from));
    ed.spec_mut()
        .require_resolution(at, from, tutti_graph::Resolution::Sample);
    ed.commit().expect("commits");
    assert!(ed.spec_mut().disconnect_events(at, from));
    assert!(!ed.spec_mut().disconnect_events(at, from), "already gone");
    assert_eq!(ed.commit(), Ok(()));
    assert!(ed.spec().required_resolution.is_empty());

    // `unrequire_resolution`: the edge stays, the requirement goes.
    let mut g = two_node_spec();
    g.connect_events(at, EventEdge::Direct(from));
    g.require_resolution(at, from, tutti_graph::Resolution::Sample);
    g.unrequire_resolution(at, from);
    let mono = || Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO);
    let shapes: Shapes = [
        (A, mono().with_events(0, 1)),
        (
            B,
            mono()
                .with_events(1, 0)
                .with_event_resolution(tutti_graph::Resolution::Block),
        ),
    ]
    .into();
    let valid = g.validate().expect("valid");
    assert!(compile(&valid, &shapes, &common::prepare(PREP), None).is_ok());
}
