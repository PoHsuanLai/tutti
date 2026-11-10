//! Audio fan-in, the views (doc 013, "Audio fan-in sums", staging item 2):
//! the latency and tail views and the reference interpreter read a **set** of
//! sources per input port and per output channel. The compiler still refuses
//! a set of two (`CompileError::FanInNotCompiled`, PR 3), so every topology
//! here is built by hand and driven through the pure passes and the
//! reference, never the executor.
//!
//! Figures are analytic: an arrival is the latest departure over every source
//! of every port, a source's delay is that arrival minus its own departure,
//! and a sum is the left fold `((+0.0 + a_0) + a_1) + ..` over the sources in
//! `Edge` order.

mod common;

use std::collections::BTreeMap;

use common::{Kind, TestNode, prepare};
use tutti_graph::{
    Edge, Feed, FeedbackFrom, InPort, InputDelay, Node, NodeKey, NodeSpec, OutPort, OutputDelay,
    Reference, Source, Topology, Transport, delays, graph_tail, latency_plan,
};
use tutti_types::{ChannelLayout, Samples, Tail};

const BLOCK: usize = 64;

fn key(n: u32) -> NodeKey {
    NodeKey(n.into())
}

fn at(node: NodeKey, port: u16) -> InPort {
    InPort { node, port }
}

fn out(node: NodeKey, port: u16) -> OutPort {
    OutPort { node, port }
}

fn direct(node: NodeKey) -> Edge {
    Edge::Direct(Source::Node(out(node, 0)))
}

fn spec(ins: u16, outs: u16) -> NodeSpec {
    NodeSpec::new(
        "n",
        ChannelLayout::from_count(ins),
        ChannelLayout::from_count(outs),
    )
}

fn node(t: &mut Topology, k: NodeKey, ins: u16, lat: usize, tail: usize) {
    t.nodes.insert(
        k,
        spec(ins, 1)
            .with_latency(Samples(lat))
            .with_tail(Tail::Finite(Samples(tail))),
    );
}

/// Sources of latency 300, 100 and 0, and a zero-latency sink `S` with two
/// audio ports.
const SLOW: NodeKey = NodeKey(1);
const MID: NodeKey = NodeKey(2);
const DRY: NodeKey = NodeKey(3);
const SINK: NodeKey = NodeKey(9);

fn sources() -> Topology {
    let mut t = Topology::default();
    node(&mut t, SLOW, 0, 300, 0);
    node(&mut t, MID, 0, 100, 0);
    node(&mut t, DRY, 0, 0, 0);
    node(&mut t, SINK, 2, 0, 0);
    t
}

fn sorted_inputs(t: &Topology) -> Vec<InputDelay<NodeKey>> {
    let mut v = delays(t).inputs().to_vec();
    v.sort();
    v
}

fn input(node: NodeKey, port: u16, feed: Feed<NodeKey>, by: Samples) -> InputDelay<NodeKey> {
    InputDelay {
        node,
        port,
        feed,
        by,
    }
}

fn output(channel: usize, feed: Feed<NodeKey>, by: Samples) -> OutputDelay<NodeKey> {
    OutputDelay { channel, feed, by }
}

/// **A fan-in port aligns each source on its own.** Port 0 sums `SLOW`
/// (300), `MID` (100) and `DRY` (0), port 1 reads `MID`: the sink arrives at
/// the latest departure over all its sources at all its ports (300), and
/// every source but the slowest is delayed by its own gap to it, so each
/// reaches the sum at the same time. The sum adds no latency of its own.
///
/// Mutation (run): `Topology::inputs` yields only a port's first source
/// (`.take(1)` on the edge list) → the `MID` and `DRY` entries on port 0 are
/// missing → fails.
#[test]
fn each_source_of_a_fan_in_port_is_delayed_to_the_arrival() {
    let mut t = sources();
    for from in [SLOW, MID, DRY] {
        t.connect(at(SINK, 0), direct(from));
    }
    t.connect(at(SINK, 1), direct(MID));
    t.set_outputs([Source::Node(out(SINK, 0))]);

    let d = delays(&t);
    assert_eq!(d.arrival(SINK), Samples(300), "the latest of all sources");
    assert_eq!(d.departure(SINK), Samples(300), "a sum has no latency");
    assert_eq!(
        sorted_inputs(&t),
        vec![
            input(SINK, 0, Feed::Node(MID), Samples(200)),
            input(SINK, 0, Feed::Node(DRY), Samples(300)),
            input(SINK, 1, Feed::Node(MID), Samples(200)),
        ]
    );
    assert_eq!(latency_plan(&t).total(), Samples(300));
}

/// A global input in a fan-in is aligned like any other source: it arrives at
/// zero, so it waits for the slowest.
///
/// Mutation (run): `Feed::Outside => continue` in `delays` → no entry for the
/// global source → fails.
#[test]
fn a_global_source_of_a_fan_in_waits_for_the_slowest() {
    let mut t = sources();
    t.inputs = ChannelLayout::MONO;
    t.connect(at(SINK, 0), direct(MID));
    t.connect(at(SINK, 0), Edge::Direct(Source::Global(0)));
    t.set_outputs([Source::Node(out(SINK, 0))]);

    assert_eq!(
        sorted_inputs(&t),
        vec![input(SINK, 0, Feed::Outside, Samples(100))]
    );
}

/// Two outputs of one node into one port are one feed, and one delay.
///
/// Mutation (run): drop the `seen` check that de-duplicates `(port, feed)` in
/// `delays` → `MID` is listed twice → fails.
#[test]
fn two_ports_of_one_node_into_a_port_are_one_delay() {
    let mut t = sources();
    t.nodes.insert(MID, spec(0, 2).with_latency(Samples(100)));
    t.connect(at(SINK, 0), direct(SLOW));
    t.connect(at(SINK, 0), Edge::Direct(Source::Node(out(MID, 0))));
    t.connect(at(SINK, 0), Edge::Direct(Source::Node(out(MID, 1))));

    assert_eq!(
        sorted_inputs(&t),
        vec![input(SINK, 0, Feed::Node(MID), Samples(200))]
    );
}

/// A feedback addend carries last block's value: it is `Feed::None`, adds
/// nothing to the arrival and is never aligned.
///
/// Mutation (run): `Topology::inputs` reads a feedback edge as the source
/// node (`Edge::Feedback(f) => Feed::Node(f.from.node)`) → the sink arrives
/// at 300 and `MID` is delayed → fails.
#[test]
fn a_feedback_addend_is_not_aligned_and_does_not_delay() {
    let mut t = sources();
    t.connect(at(SINK, 0), direct(MID));
    t.connect(
        at(SINK, 0),
        Edge::Feedback(FeedbackFrom::new(out(SLOW, 0), Samples(BLOCK))),
    );
    let d = delays(&t);
    assert_eq!(d.arrival(SINK), Samples(100));
    assert!(d.inputs().is_empty());
}

/// **A fan-in output channel aligns each source to the output time**, and the
/// channel's arrival is its latest source's departure. Channel 0 sums `SLOW`
/// and `DRY`, channel 1 is `MID`: the output time is 300, `DRY` and `MID` are
/// delayed by their own gaps, and channel 1's pre-roll is the gap of its
/// latest source.
///
/// Mutation (run): `delays` takes a channel's arrival from its *last* source
/// instead of the latest (`channel_arrivals[ch] = feed_departure(feed)`) →
/// with the sources listed `SLOW`, `DRY` channel 0 arrives at 0, the total
/// falls to 100 → fails.
#[test]
fn each_source_of_a_fan_in_output_is_delayed_to_the_output_time() {
    let mut t = sources();
    t.set_outputs([Source::Node(out(SLOW, 0)), Source::Node(out(MID, 0))]);
    t.outputs[0].push(Source::Node(out(DRY, 0)));

    let d = delays(&t);
    assert_eq!(
        d.outputs(),
        &[
            output(0, Feed::Node(DRY), Samples(300)),
            output(1, Feed::Node(MID), Samples(200)),
        ]
    );
    let c = d.compensation();
    assert_eq!(c.total(), Samples(300));
    assert_eq!(c.channels(), &[Samples(0), Samples(200)]);
}

/// **A sum passes the longest tail among its sources**, not their sum, at a
/// port and at an output channel, and the sink's own tail follows. Tails
/// exclude latency, so the sources' latencies change nothing.
///
/// Mutation (run): `Topology::inputs` and `outputs` yield only the first
/// source → a port reads 2000 and the output 2000 → fails (the longer source
/// has the higher key).
#[test]
fn the_tail_of_a_sum_is_the_longest_of_its_sources() {
    let mut t = Topology::default();
    node(&mut t, key(1), 0, 0, 2000);
    node(&mut t, key(2), 0, 700, 3000);
    node(&mut t, SINK, 1, 0, 100);
    t.connect(at(SINK, 0), direct(key(1)));
    t.connect(at(SINK, 0), direct(key(2)));
    t.set_outputs([Source::Node(out(SINK, 0))]);
    assert_eq!(graph_tail(&t).samples(), Some(Samples(3100)));

    // The same two into one output channel.
    t.set_outputs([Source::Node(out(key(1), 0))]);
    t.outputs[0].push(Source::Node(out(key(2), 0)));
    assert_eq!(graph_tail(&t).samples(), Some(Samples(3000)));
}

// ---- the reference interpreter -------------------------------------------

const A: NodeKey = NodeKey(1);
const B: NodeKey = NodeKey(2);
const C: NodeKey = NodeKey(3);
/// A unit-gain, one-in one-out node: its output is its input.
const PASS: NodeKey = NodeKey(8);
const LAG: NodeKey = NodeKey(7);
const LAG_FRAMES: usize = 8;

fn constant(v: f32) -> Kind {
    Kind::Const { value: v, width: 1 }
}

/// Renders `blocks` blocks of `BLOCK` frames of `t` through the reference,
/// one output channel, and returns the concatenated output.
fn render(t: Topology, kinds: BTreeMap<NodeKey, Kind>, input: &[f32], blocks: usize) -> Vec<f32> {
    let valid = tutti_graph::GraphSpec::new(t).validate().expect("valid");
    let units: BTreeMap<NodeKey, Box<dyn Node>> = kinds
        .iter()
        .map(|(&k, kind)| (k, Box::new(TestNode::new(kind.clone())) as Box<dyn Node>))
        .collect();
    let mut reference = Reference::new(prepare(BLOCK));
    reference.set_graph(&valid, units);
    let mut all = Vec::new();
    for b in 0..blocks {
        let mut block = vec![0.0f32; BLOCK];
        let ins = if input.is_empty() {
            Vec::new()
        } else {
            vec![&input[b * BLOCK..(b + 1) * BLOCK]]
        };
        reference.process(BLOCK, &Transport::default(), &ins, &mut [&mut block[..]]);
        all.extend(block);
    }
    all
}

/// The topology of `PASS` summing `sources` (given in this order, raw: the
/// reference sorts them) into the one output.
fn summed(list: &[(NodeKey, f32)]) -> (Topology, BTreeMap<NodeKey, Kind>) {
    let mut t = Topology::default();
    let mut kinds = BTreeMap::new();
    t.nodes.insert(PASS, spec(1, 1));
    kinds.insert(
        PASS,
        Kind::Gain {
            gain: 1.0,
            width: 1,
        },
    );
    let mut edges = Vec::new();
    for &(k, v) in list {
        t.nodes.insert(k, spec(0, 1));
        kinds.insert(k, constant(v));
        edges.push(direct(k));
    }
    t.edges.insert(at(PASS, 0), edges);
    t.set_outputs([Source::Node(out(PASS, 0))]);
    (t, kinds)
}

/// **The sum is the left fold from `+0.0` over the sources in `Edge` order**,
/// whatever order the spec lists them in. `1e8, -1e8, 1.0` in key order is
/// `((0 + 1e8) - 1e8) + 1 = 1`; in any other association or order the `1.0`
/// is lost against `1e8`.
///
/// Mutation (run): do not sort the sources in `sorted` → listed `C, B, A` the
/// fold is `((0 + 1) - 1e8) + 1e8 = 0` → fails. Mutation: fold the list in
/// reverse → the same → fails.
#[test]
fn the_reference_sums_in_edge_order_whatever_order_they_are_listed() {
    let (a, b, c) = (1.0e8f32, -1.0e8f32, 1.0f32);
    let want = ((0.0f32 + a) + b) + c;
    assert_eq!(want, 1.0, "the figures are order-dependent");
    let (t, kinds) = summed(&[(C, c), (B, b), (A, a)]);
    let got = render(t, kinds, &[], 1);
    assert!(
        got.iter().all(|&x| x.to_bits() == want.to_bits()),
        "{got:?}"
    );
}

/// **The accumulator starts at `+0.0`**: two `-0.0` sources sum to `+0.0`,
/// while a single source is aliased, so its `-0.0` stays.
///
/// Mutation (run): start the fold from the first addend instead of `+0.0`
/// (`acc = addends[0].clone()`, then add the rest) → `-0.0` survives the sum
/// → fails.
#[test]
fn the_sum_starts_from_positive_zero_and_one_source_is_an_alias() {
    let (t, kinds) = summed(&[(A, -0.0), (B, -0.0)]);
    let got = render(t, kinds, &[], 1);
    assert!(got.iter().all(|x| x.to_bits() == 0.0f32.to_bits()));

    let (t, kinds) = summed(&[(A, -0.0)]);
    let got = render(t, kinds, &[], 1);
    assert!(got.iter().all(|x| x.to_bits() == (-0.0f32).to_bits()));
}

/// An impulse into a latent path and straight into the sum: it arrives
/// **once**, aligned, `LAG_FRAMES` late, including across a block boundary.
fn impulse_input(frame: usize, blocks: usize) -> Vec<f32> {
    let mut x = vec![0.0f32; BLOCK * blocks];
    x[frame] = 1.0;
    x
}

fn lag_into_pass() -> (Topology, BTreeMap<NodeKey, Kind>) {
    let mut t = Topology {
        inputs: ChannelLayout::MONO,
        ..Topology::default()
    };
    let mut kinds = BTreeMap::new();
    t.nodes
        .insert(LAG, spec(1, 1).with_latency(Samples(LAG_FRAMES)));
    kinds.insert(
        LAG,
        Kind::Lag {
            latency: LAG_FRAMES,
        },
    );
    t.connect(at(LAG, 0), Edge::Direct(Source::Global(0)));
    t.nodes.insert(PASS, spec(1, 1));
    kinds.insert(
        PASS,
        Kind::Gain {
            gain: 1.0,
            width: 1,
        },
    );
    (t, kinds)
}

/// **Per-source PDC at an input port.** The impulse goes down `LAG` (8
/// frames of latency) and straight from the global input into the same port;
/// the direct source is delayed to the sink's arrival, so both meet at frame
/// 8 + 60 = 68 (the next block) and sum to 2.0, and nothing is heard at 60.
///
/// Mutation (run): skip the delay for a `Source::Global` in the reference's
/// delay derivation (`Source::Global(_) => continue`) → 1.0 at frame 60 and
/// 1.0 at 68 → fails.
#[test]
fn a_fan_in_input_port_aligns_its_sources_across_blocks() {
    let (mut t, kinds) = lag_into_pass();
    t.connect(at(PASS, 0), direct(LAG));
    t.connect(at(PASS, 0), Edge::Direct(Source::Global(0)));
    t.set_outputs([Source::Node(out(PASS, 0))]);
    let input = impulse_input(60, 2);
    let got = render(t, kinds, &input, 2);
    let mut want = vec![0.0f32; 2 * BLOCK];
    want[60 + LAG_FRAMES] = 2.0;
    assert_eq!(got, want);
}

/// **Per-source PDC at a global output**: the same sources summed straight
/// onto the output channel, each delayed to the output time first.
///
/// Mutation (run): as above, in the output's delay derivation → fails.
#[test]
fn a_fan_in_output_channel_aligns_its_sources() {
    let (mut t, kinds) = lag_into_pass();
    t.nodes.remove(&PASS);
    let mut kinds = kinds;
    kinds.remove(&PASS);
    t.set_outputs([Source::Node(out(LAG, 0))]);
    t.outputs[0].push(Source::Global(0));
    let input = impulse_input(3, 1);
    let got = render(t, kinds, &input, 1);
    let mut want = vec![0.0f32; BLOCK];
    want[3 + LAG_FRAMES] = 2.0;
    assert_eq!(got, want);
}

/// **A feedback addend sorts after every direct one**, and reads last
/// block's value: with `1e8` and `-1e8` direct and a feedback of `1.0`, the
/// second block is `((0 + 1e8) - 1e8) + 1 = 1`, where a feedback summed first
/// would lose the `1.0`. The first block reads the silent line: 0.
///
/// Mutation (run): sum the addends in reverse, which puts the feedback first
/// → `((0 + 1) - 1e8) + 1e8 = 0` → fails.
#[test]
fn a_feedback_addend_is_summed_last() {
    let (mut t, mut kinds) = summed(&[(A, 1.0e8), (B, -1.0e8)]);
    t.nodes.insert(C, spec(0, 1));
    kinds.insert(C, constant(1.0));
    t.connect(
        at(PASS, 0),
        Edge::Feedback(FeedbackFrom::one_block(out(C, 0), Samples(BLOCK))),
    );
    let got = render(t, kinds, &[], 2);
    let (first, second) = got.split_at(BLOCK);
    assert!(first.iter().all(|&x| x == 0.0), "{first:?}");
    let want = (((0.0f32 + 1.0e8) + -1.0e8) + 1.0).to_bits();
    assert!(second.iter().all(|x| x.to_bits() == want), "{second:?}");
}
