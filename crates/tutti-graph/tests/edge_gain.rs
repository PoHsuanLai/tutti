//! Edge gain, stage G1 (doc 013, "Edge gain: a mixer is a weighted sum"): an
//! edge with an entry in `GraphSpec::gains` is a **weighted lane** of its
//! sink's sum, `acc = acc + w * a`, with the weight moved by
//! `Editor::set_edge_gain` and ramped across the block.
//!
//! Every figure is analytic: the expected value is the fold, or the ramp,
//! written out in the test. A render is compared with another only as the doc
//! prescribes: the compiled executor against the reference interpreter
//! (`Pair`), and the parallel executor against the serial one.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use common::{Kind, Pair, TestNode, bits, input_signal, prepare};
use tutti_graph::{
    CommitError, Cx, Edge, EdgeGain, EdgeId, Editor, Executor, ForkByClone, ForkMode, ForkTarget,
    GraphInvalid, GraphSpec, InPort, Io, Node, NodeKey, NodeSpec, Op, OutPort, Plan, Prepare,
    Renderer, Shape, Sink, Source, Status, Topology, Transport, Unforkable, ValidGraph, compile,
};
use tutti_types::{Amplitude, ChannelLayout};

const BLOCK: usize = 64;

fn key(n: u64) -> NodeKey {
    NodeKey(n)
}

fn at(node: u64, port: u16) -> InPort {
    InPort {
        node: key(node),
        port,
    }
}

fn out(node: u64) -> Source {
    Source::Node(OutPort {
        node: key(node),
        port: 0,
    })
}

fn direct(node: u64) -> Edge {
    Edge::Direct(out(node))
}

fn dc(value: f32) -> Kind {
    Kind::Const { value, width: 1 }
}

fn amp(v: f32) -> Amplitude {
    Amplitude::new(v)
}

/// Edge `node → (sink, 0)` of a pass-through sink.
fn port_edge(sink: u64, from: u64) -> EdgeId {
    EdgeId {
        sink: Sink::Port(at(sink, 0)),
        from: direct(from),
    }
}

/// Edge `node → output 0`.
fn output_edge(from: u64) -> EdgeId {
    EdgeId {
        sink: Sink::Output(0),
        from: direct(from),
    }
}

fn spec_of(kind: &Kind) -> NodeSpec {
    let s = TestNode::new(kind.clone()).shape();
    NodeSpec::new("test", s.audio_in(), s.audio_out())
        .with_latency(s.latency.samples())
        .with_tail(s.tail)
}

fn nodes(kinds: &[Kind]) -> (Topology, BTreeMap<NodeKey, Kind>) {
    let mut t = Topology::default();
    let mut map = BTreeMap::new();
    for (i, k) in kinds.iter().enumerate() {
        t.nodes.insert(key(i as u64 + 1), spec_of(k));
        map.insert(key(i as u64 + 1), k.clone());
    }
    (t, map)
}

/// `values.len()` constants keyed `1..=n` summed at the one port of a
/// pass-through keyed `n + 1` (`at_port`), or at output channel 0; and the
/// edge of source `k` (1-based).
struct Fan {
    topology: Topology,
    kinds: BTreeMap<NodeKey, Kind>,
    at_port: bool,
    /// How many sources (keys `1..=n`).
    n: u64,
}

impl Fan {
    fn new(values: &[f32], at_port: bool) -> Self {
        let mut kinds: Vec<Kind> = values.iter().map(|&v| dc(v)).collect();
        if at_port {
            kinds.push(Kind::Thru { width: 1 });
        }
        let (mut t, map) = nodes(&kinds);
        let n = values.len() as u64;
        if at_port {
            for k in 1..=n {
                t.connect(at(n + 1, 0), direct(k));
            }
            t.set_outputs(vec![out(n + 1)]);
        } else {
            t.outputs = vec![(1..=n).map(out).collect()];
        }
        Self {
            topology: t,
            kinds: map,
            at_port,
            n,
        }
    }

    fn edge(&self, k: u64) -> EdgeId {
        if self.at_port {
            port_edge(self.n + 1, k)
        } else {
            output_edge(k)
        }
    }

    /// The spec with `weights[k - 1]` on source `k`.
    fn spec(&self, weights: &[f32]) -> ValidGraph {
        let mut spec = GraphSpec::new(self.topology.clone());
        for (k, &w) in weights.iter().enumerate() {
            spec.set_edge_gain(self.edge(k as u64 + 1), EdgeGain::new(amp(w)));
        }
        spec.validate().expect("valid")
    }

    fn plan(&self, graph: &ValidGraph) -> Plan {
        compile(
            graph,
            &common::shapes_of(&self.kinds),
            &prepare(BLOCK),
            None,
        )
        .expect("compiles")
        .0
    }
}

fn weighted_ops(plan: &Plan) -> usize {
    plan.ops()
        .iter()
        .filter(|o| matches!(o, Op::WeightedSum { .. }))
        .count()
}

fn plain_sums(plan: &Plan) -> usize {
    plan.ops()
        .iter()
        .filter(|o| matches!(o, Op::Sum { .. }))
        .count()
}

/// Render `blocks` blocks of `BLOCK` frames of `graph` on `workers`
/// participants, asserting each block is the reference interpreter's.
fn render(
    graph: &ValidGraph,
    kinds: &BTreeMap<NodeKey, Kind>,
    workers: usize,
    blocks: usize,
) -> Vec<Vec<f32>> {
    let mut pair = Pair::with_workers(BLOCK, workers);
    pair.switch(graph, kinds);
    let mut got: Vec<Vec<f32>> = vec![Vec::new(); pair.outputs];
    let mut frame = 0;
    for _ in 0..blocks {
        let input = input_signal(frame, BLOCK);
        let (a, b) = pair.block(BLOCK, &input);
        assert_eq!(bits(&a), bits(&b), "executor and reference diverge");
        for (g, a) in got.iter_mut().zip(a) {
            g.extend(a);
        }
        frame += BLOCK as u64;
    }
    got
}

/// Serial and parallel renders, which must be bit for bit the same.
fn render_both(
    graph: &ValidGraph,
    kinds: &BTreeMap<NodeKey, Kind>,
    blocks: usize,
) -> Vec<Vec<f32>> {
    let serial = render(graph, kinds, 1, blocks);
    let parallel = render(graph, kinds, 4, blocks);
    assert_eq!(
        bits(&serial),
        bits(&parallel),
        "parallel differs from serial"
    );
    serial
}

/// `((+0.0 + w_0 * a_0) + w_1 * a_1) + ..`: a multiply and an add per lane,
/// two roundings (Rust never contracts them).
fn weighted_fold(values: &[f32], weights: &[f32]) -> f32 {
    values
        .iter()
        .zip(weights)
        .fold(0.0f32, |acc, (&a, &w)| acc + w * a)
}

/// The same with a fused multiply-add: the figure a kernel that fused would
/// produce.
fn fused_fold(values: &[f32], weights: &[f32]) -> f32 {
    values
        .iter()
        .zip(weights)
        .fold(0.0f32, |acc, (&a, &w)| w.mul_add(a, acc))
}

// ---------------------------------------------------------------------------
// The kernel: the fold.
// ---------------------------------------------------------------------------

/// A weighted sum is `((0 + w0*a) + w1*b) + w2*c`, exactly, in source order,
/// at a port and at an output channel, serial, parallel and reference.
///
/// The figures show the two roundings: the fold is not the fused one (asserted
/// below, so the test can see a fused kernel), and not the pairwise one.
#[test]
fn a_weighted_sum_is_the_documented_fold() {
    let values = [0.3f32, 0.2, 0.9];
    let weights = [0.7f32, 0.35, 0.35];
    let want = weighted_fold(&values, &weights);
    assert_ne!(
        want.to_bits(),
        fused_fold(&values, &weights).to_bits(),
        "the figures separate a fused multiply-add from two roundings"
    );
    for at_port in [false, true] {
        let fan = Fan::new(&values, at_port);
        let graph = fan.spec(&weights);
        assert_eq!(weighted_ops(&fan.plan(&graph)), 1);
        for channel in render_both(&graph, &fan.kinds, 2) {
            assert!(
                channel.iter().all(|&x| x.to_bits() == want.to_bits()),
                "at_port {at_port}: {:?} want {want}",
                &channel[..2]
            );
        }
    }
}

/// A wide weighted fan-in is a chain and still one flat fold: 100 lanes with
/// weights of many kinds (unity, zero, negative, fractions), at both sinks,
/// is the fold written out over all of them. Some lanes are plain (no entry),
/// as the chain's own continuation addend is.
#[test]
fn a_wide_weighted_fan_in_is_one_flat_fold() {
    let n = 100usize;
    let values: Vec<f32> = (0..n)
        .map(|k| ((k * 37 % 11) as f32 + 1.0) * 0.1 * 10f32.powi((k % 5) as i32))
        .collect();
    let weights: Vec<f32> = (0..n)
        .map(|k| match k % 7 {
            0 => 1.0,
            1 => 0.0,
            2 => -0.5,
            3 => 0.3,
            4 => 1.75,
            5 => -1.0,
            _ => 0.1,
        })
        .collect();
    // Every third lane is plain: weight 1.0 and no entry.
    let entries: Vec<bool> = (0..n).map(|k| k % 3 != 0).collect();
    let applied: Vec<f32> = weights
        .iter()
        .zip(&entries)
        .map(|(&w, &e)| if e { w } else { 1.0 })
        .collect();
    let want = weighted_fold(&values, &applied);
    for at_port in [false, true] {
        let fan = Fan::new(&values, at_port);
        let mut spec = GraphSpec::new(fan.topology.clone());
        for k in 0..n {
            if entries[k] {
                spec.set_edge_gain(fan.edge(k as u64 + 1), EdgeGain::new(amp(weights[k])));
            }
        }
        let graph = spec.validate().expect("valid");
        let plan = fan.plan(&graph);
        // 64, then 63 per op: two ops, both carrying a weighted lane.
        assert_eq!(weighted_ops(&plan) + plain_sums(&plan), 2);
        for channel in render_both(&graph, &fan.kinds, 1) {
            assert!(
                channel.iter().all(|&x| x.to_bits() == want.to_bits()),
                "at_port {at_port}"
            );
        }
    }
}

/// A weight of exactly `1.0` is the plain sum, bit for bit, for fan-ins of 2,
/// 8 and 100 (the last a chain): the same values rendered with an entry of
/// `1.0` on every edge and with none.
#[test]
fn unity_weights_equal_the_unweighted_sum() {
    for n in [2usize, 8, 100] {
        let values: Vec<f32> = (0..n)
            .map(|k| ((k * 29 % 13) as f32 - 6.0) * 0.173 + 0.01 * k as f32)
            .collect();
        for at_port in [false, true] {
            let fan = Fan::new(&values, at_port);
            let plain = GraphSpec::new(fan.topology.clone())
                .validate()
                .expect("valid");
            let unity = fan.spec(&vec![1.0; n]);
            assert_eq!(weighted_ops(&fan.plan(&plain)), 0);
            assert!(weighted_ops(&fan.plan(&unity)) >= 1);
            let a = render_both(&plain, &fan.kinds, 3);
            let b = render_both(&unity, &fan.kinds, 3);
            assert_eq!(bits(&a), bits(&b), "n = {n}, at_port {at_port}");
        }
    }
}

/// The one-edge corner: a plain edge is an alias and keeps a `-0.0` sample; a
/// **unity entry** on the same edge is an `Op::WeightedSum` of one addend,
/// `+0.0 + 1.0 * a`, which turns that `-0.0` into `+0.0`. Nothing audible
/// differs, and this test pins the difference rather than hiding it (doc 013,
/// "Edge gain", "Weight 1.0 is bit-identical to the plain sum").
#[test]
fn a_unity_entry_on_a_one_edge_sink_turns_negative_zero_positive() {
    let neg = -0.0f32;
    for at_port in [false, true] {
        let fan = Fan::new(&[neg], at_port);
        let plain = GraphSpec::new(fan.topology.clone())
            .validate()
            .expect("valid");
        assert_eq!(
            weighted_ops(&fan.plan(&plain)) + plain_sums(&fan.plan(&plain)),
            0
        );
        for ch in render_both(&plain, &fan.kinds, 1) {
            assert!(
                ch.iter().all(|x| x.to_bits() == neg.to_bits()),
                "the plain alias keeps -0.0 (at_port {at_port})"
            );
        }
        let unity = fan.spec(&[1.0]);
        assert_eq!(weighted_ops(&fan.plan(&unity)), 1, "a sum of one addend");
        for ch in render_both(&unity, &fan.kinds, 1) {
            assert!(
                ch.iter().all(|x| x.to_bits() == 0),
                "the unity entry runs +0.0 + a (at_port {at_port})"
            );
        }
    }
    // Not in place: the sole addend is also read elsewhere, so the sum writes
    // a fresh slot.
    let mut fan = Fan::new(&[neg], true);
    fan.topology
        .nodes
        .insert(key(3), spec_of(&Kind::Thru { width: 1 }));
    fan.kinds.insert(key(3), Kind::Thru { width: 1 });
    fan.topology.connect(at(3, 0), direct(1));
    fan.topology.outputs.push(vec![out(3)]);
    let graph = fan.spec(&[1.0]);
    let got = render_both(&graph, &fan.kinds, 1);
    assert!(got[0].iter().all(|x| x.to_bits() == 0), "the weighted sink");
    assert!(
        got[1].iter().all(|x| x.to_bits() == neg.to_bits()),
        "the other reader of the source keeps -0.0"
    );
}

// ---------------------------------------------------------------------------
// Ramps.
// ---------------------------------------------------------------------------

/// `w[i]` as the doc writes it.
fn ramp(from: f32, to: f32, frames: usize) -> Vec<f32> {
    (0..frames)
        .map(|i| from + (to - from) * ((i + 1) as f32 / frames as f32))
        .collect()
}

/// One weighted source of `1.0` into the output: the output *is* the weight,
/// `+0.0 + w[i] * 1.0`.
fn one_lane() -> (Fan, ValidGraph) {
    let fan = Fan::new(&[1.0], false);
    let graph = fan.spec(&[0.5]);
    (fan, graph)
}

/// A move ramps across one block, pinned from the formula: the first block
/// has no ramp (it starts at its value); the next ramps from the first's
/// weight to the new one across **that block's own length** (a short block
/// here); a block with no move is static; a move after a ramp starts from the
/// weight the ramp was *given* (its target), not the last sample it computed.
/// Serial, parallel and reference.
#[test]
fn a_move_ramps_across_the_block_from_the_last_weight() {
    // 0.7 + (0.15 - 0.7) * 1.0 is not 0.15 in f32: the target and the last
    // sample the ramp computes differ, so a lane that carried the sample would
    // ramp again in the next block.
    let (a, b, c) = (0.7f32, 0.15f32, 0.25f32);
    assert_ne!(
        ramp(a, b, 40)[39].to_bits(),
        b.to_bits(),
        "the figures separate the target from the last sample"
    );
    for workers in [1usize, 4] {
        let (fan, _) = one_lane();
        let graph = fan.spec(&[a]);
        let mut pair = Pair::with_workers(BLOCK, workers);
        pair.switch(&graph, &fan.kinds);
        let id = fan.edge(1);
        let run = |pair: &mut Pair, frames: usize| {
            let (x, y) = pair.block(frames, &input_signal(0, frames));
            assert_eq!(bits(&x), bits(&y), "executor and reference");
            x.into_iter().next().expect("one output")
        };
        // Block 1: no ramp, the weight itself.
        assert!(run(&mut pair, BLOCK).iter().all(|&x| x == a), "first block");
        // Block 2: a move, a short block.
        pair.set_edge_gain(id, amp(b));
        let got = run(&mut pair, 40);
        for (i, (&g, w)) in got.iter().zip(ramp(a, b, 40)).enumerate() {
            assert_eq!(g.to_bits(), (0.0 + w * 1.0).to_bits(), "ramp, frame {i}");
        }
        // Block 3: nothing moved: static at the target, exactly.
        assert!(
            run(&mut pair, BLOCK).iter().all(|&x| x == b),
            "static after a ramp"
        );
        // Block 4: a second move ramps from the target.
        pair.set_edge_gain(id, amp(c));
        let got = run(&mut pair, BLOCK);
        for (i, (&g, w)) in got.iter().zip(ramp(b, c, BLOCK)).enumerate() {
            assert_eq!(g.to_bits(), (0.0 + w * 1.0).to_bits(), "second ramp, {i}");
        }
        // A move to the weight it already has is no move.
        pair.set_edge_gain(id, amp(c));
        assert!(run(&mut pair, BLOCK).iter().all(|&x| x == c));
    }
}

/// A ramping lane adds `w[i] * a` as a multiply and an add too, never fused:
/// a static lane then a ramping one, the second block moving the second weight
/// across the block, equal to the per-sample fold written out here (and not to
/// the fused one, which the figures separate on some samples).
#[test]
fn a_ramping_lane_is_not_fused() {
    let (values, first, from, to) = ([0.3f32, 0.9], 0.7f32, 0.35f32, 1.3f32);
    let head = 0.0f32 + first * values[0];
    let w = ramp(from, to, BLOCK);
    let want: Vec<f32> = w.iter().map(|&w| head + w * values[1]).collect();
    let fused: Vec<f32> = w.iter().map(|&w| w.mul_add(values[1], head)).collect();
    assert_ne!(
        bits(std::slice::from_ref(&want)),
        bits(&[fused]),
        "the figures separate"
    );
    for at_port in [false, true] {
        let fan = Fan::new(&values, at_port);
        for workers in [1usize, 4] {
            let mut pair = Pair::with_workers(BLOCK, workers);
            pair.switch(&fan.spec(&[first, from]), &fan.kinds);
            let input = input_signal(0, BLOCK);
            pair.block(BLOCK, &input);
            pair.set_edge_gain(fan.edge(2), amp(to));
            let (a, b) = pair.block(BLOCK, &input);
            assert_eq!(bits(&a), bits(&b), "executor and reference");
            assert_eq!(
                bits(&a[..1]),
                bits(std::slice::from_ref(&want)),
                "at_port {at_port}"
            );
        }
    }
}

/// A lane's state is its edge's, so it survives a commit that keeps the edge
/// (here one that adds a source to the same sink and so recompiles the sum):
/// the move that comes with the commit ramps from the weight the lane had.
/// A commit that drops the entry forgets it, and one that adds it back starts
/// at its value.
#[test]
fn a_lane_survives_a_commit_that_keeps_its_edge() {
    let values = [1.0f32, 0.5];
    let fan = Fan::new(&values, false);
    for workers in [1usize, 4] {
        let mut pair = Pair::with_workers(BLOCK, workers);
        let first = fan.spec(&[0.25, 1.0]);
        pair.switch(&first, &fan.kinds);
        let step = |pair: &mut Pair| {
            let (x, y) = pair.block(BLOCK, &input_signal(0, BLOCK));
            assert_eq!(bits(&x), bits(&y), "executor and reference");
            x
        };
        let b1 = step(&mut pair);
        assert!(b1[0].iter().all(|&x| x == 0.25 * 1.0 + 0.5 * 1.0));

        // Same wiring, a new base for each lane: a commit (a recompile, with
        // a plan of its own) that keeps both edges, so both ramp from the
        // weight they had.
        let second = fan.spec(&[2.0, 0.5]);
        pair.switch(&second, &fan.kinds);
        let b2 = step(&mut pair);
        let (w1, w2) = (ramp(0.25, 2.0, BLOCK), ramp(1.0, 0.5, BLOCK));
        for (i, (&g, (&w1, &w2))) in b2[0].iter().zip(w1.iter().zip(&w2)).enumerate() {
            let want = (0.0 + w1 * 1.0) + w2 * 0.5;
            assert_eq!(g.to_bits(), want.to_bits(), "carried ramp, frame {i}");
        }

        // Drop lane 1's entry (the edge stays): plain again.
        let third = {
            let mut spec = GraphSpec::new(fan.topology.clone());
            spec.set_edge_gain(fan.edge(2), EdgeGain::new(amp(0.5)));
            spec.validate().expect("valid")
        };
        pair.switch(&third, &fan.kinds);
        step(&mut pair);
        // Add it back at a new value: a new lane, no ramp.
        let fourth = fan.spec(&[4.0, 0.5]);
        pair.switch(&fourth, &fan.kinds);
        let b4 = step(&mut pair);
        let want: f32 = (0.0 + 4.0 * 1.0) + 0.5 * 0.5;
        assert!(
            b4[0].iter().all(|&x| x.to_bits() == want.to_bits()),
            "a re-added entry starts at its value"
        );
    }
}

// ---------------------------------------------------------------------------
// `set_edge_gain` and the cell.
// ---------------------------------------------------------------------------

/// A running editor and executor over two constants, `1.0` (key 1) and `2.0`
/// (key 2), summed at the output, with an entry on the first.
struct Rig {
    ed: Editor,
    exec: Executor,
}

impl Rig {
    fn new(base: f32) -> Self {
        let (mut ed, mut exec) = Editor::new(prepare(BLOCK));
        ed.insert(key(1), "one", Unforkable(TestNode::new(dc(1.0))));
        ed.insert(key(2), "two", Unforkable(TestNode::new(dc(2.0))));
        ed.spec_mut().topology.outputs = vec![vec![out(1), out(2)]];
        ed.spec_mut()
            .set_edge_gain(output_edge(1), EdgeGain::new(amp(base)));
        ed.commit().expect("commits");
        exec.apply_pending();
        ed.collect();
        Self { ed, exec }
    }

    fn block(&mut self) -> Vec<f32> {
        let mut out = vec![0.0f32; BLOCK];
        self.exec
            .process(BLOCK, &Transport::default(), &[], &mut [out.as_mut_slice()]);
        self.ed.collect();
        out
    }

    fn plan(&self) -> Arc<Plan> {
        Arc::clone(self.ed.base().expect("a plan"))
    }
}

/// `set_edge_gain` is a control: it leaves the plan untouched (the very same
/// plan `Arc`, and a commit after it sends nothing), and its value lands at the
/// next block, ramped. A change of the entry's `base` or `range` through the
/// spec is the same; adding or removing an entry is a recompile.
#[test]
fn set_edge_gain_leaves_the_plan_untouched_and_lands_next_block() {
    let mut rig = Rig::new(0.5);
    let plan = rig.plan();
    assert!(
        rig.block().iter().all(|&x| x == (0.0 + 0.5 * 1.0) + 2.0),
        "the first block"
    );

    rig.ed
        .set_edge_gain(output_edge(1), amp(1.5))
        .expect("an edge with an entry");
    assert_eq!(rig.ed.edge_gain(output_edge(1)), Some(amp(1.5)));
    rig.ed.commit().expect("commits");
    assert!(Arc::ptr_eq(&plan, &rig.plan()), "no new plan");
    let got = rig.block();
    for (i, (&g, w)) in got.iter().zip(ramp(0.5, 1.5, BLOCK)).enumerate() {
        let want = (0.0 + w * 1.0) + 2.0;
        assert_eq!(g.to_bits(), want.to_bits(), "frame {i}");
    }

    // The same through the spec: base and range, then a commit.
    {
        let g = rig
            .ed
            .spec_mut()
            .gains
            .get_mut(&output_edge(1))
            .expect("entry");
        g.base = amp(0.25);
        g.range = tutti_graph::ParamRange::new(-2.0, 2.0);
    }
    rig.ed.commit().expect("commits");
    assert!(Arc::ptr_eq(&plan, &rig.plan()), "base and range: no plan");
    let got = rig.block();
    assert_eq!(got[BLOCK - 1], (0.0 + 0.25 * 1.0) + 2.0, "landed");
    assert!(got[0] > got[BLOCK - 1], "ramping down");

    // An entry added or removed is structural.
    rig.ed
        .set_edge_gain(output_edge(2), amp(3.0))
        .expect("the edge exists");
    rig.ed.commit().expect("commits");
    let with_both = rig.plan();
    assert!(!Arc::ptr_eq(&plan, &with_both), "a new entry recompiles");
    rig.exec.apply_pending();
    let got = rig.block();
    assert_eq!(got[0], (0.0 + 0.25 * 1.0) + 3.0 * 2.0, "its first block");
    rig.ed.spec_mut().clear_edge_gain(output_edge(2));
    rig.ed.commit().expect("commits");
    assert!(!Arc::ptr_eq(&with_both, &rig.plan()), "a removed one too");
}

/// `Editor::set_edge_gain` refuses what the commit would, changing nothing: an
/// edge that does not exist, and a base that is not finite.
#[test]
fn set_edge_gain_refuses_what_a_commit_would() {
    let mut rig = Rig::new(0.5);
    let missing = EdgeId {
        sink: Sink::Output(0),
        from: direct(9),
    };
    assert_eq!(
        rig.ed.set_edge_gain(missing, amp(1.0)),
        Err(GraphInvalid::GainWithoutEdge { id: missing })
    );
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let err = rig
            .ed
            .set_edge_gain(output_edge(1), amp(bad))
            .expect_err("not finite");
        assert!(matches!(err, GraphInvalid::BadEdgeGain { .. }), "{bad}");
    }
    assert_eq!(
        rig.ed.edge_gain(output_edge(1)),
        Some(amp(0.5)),
        "unchanged"
    );
}

// ---------------------------------------------------------------------------
// Validate, and what removes an entry.
// ---------------------------------------------------------------------------

/// `validate` refuses an entry whose edge does not exist (a port and an output
/// edge, a feedback edge's source gone) and one with a base that is not finite
/// or a range with a NaN bound; an infinite bound is fine.
#[test]
fn validate_refuses_a_malformed_entry() {
    let fan = Fan::new(&[1.0, 2.0], true);
    let valid = |spec: &GraphSpec| spec.validate();
    let base = || GraphSpec::new(fan.topology.clone());

    let mut spec = base();
    let stranger = port_edge(3, 7);
    spec.set_edge_gain(stranger, EdgeGain::default());
    assert_eq!(
        valid(&spec).unwrap_err(),
        vec![GraphInvalid::GainWithoutEdge { id: stranger }]
    );

    let mut spec = base();
    // The sink's output channel is fed by node 3, not by source 1.
    spec.set_edge_gain(output_edge(1), EdgeGain::default());
    assert!(matches!(
        valid(&spec).unwrap_err()[..],
        [GraphInvalid::GainWithoutEdge { .. }]
    ));

    for bad in [f32::NAN, f32::INFINITY] {
        let mut spec = base();
        spec.set_edge_gain(fan.edge(1), EdgeGain::new(amp(bad)));
        assert!(matches!(
            valid(&spec).unwrap_err()[..],
            [GraphInvalid::BadEdgeGain { .. }]
        ));
    }
    let mut spec = base();
    spec.set_edge_gain(
        fan.edge(1),
        EdgeGain {
            base: amp(1.0),
            range: tutti_graph::ParamRange::new(f32::NAN, 1.0),
            source: None,
        },
    );
    assert!(matches!(
        valid(&spec).unwrap_err()[..],
        [GraphInvalid::BadEdgeGain { .. }]
    ));
    let mut spec = base();
    spec.set_edge_gain(
        fan.edge(1),
        EdgeGain {
            base: amp(-2.0),
            range: tutti_graph::ParamRange::UNBOUNDED,
            source: None,
        },
    );
    assert!(valid(&spec).is_ok(), "a negative weight and an open range");
}

/// `GraphSpec::disconnect` and `disconnect_output` remove the edge's entry
/// with the edge, and `Editor::remove` the entries of the edges that went
/// with a node; a stale entry is what `validate` would refuse.
#[test]
fn removing_an_edge_removes_its_entry() {
    let fan = Fan::new(&[1.0, 2.0], true);
    let mut spec = GraphSpec::new(fan.topology.clone());
    spec.set_edge_gain(fan.edge(1), EdgeGain::default());
    spec.set_edge_gain(fan.edge(2), EdgeGain::default());
    assert!(spec.disconnect(at(3, 0), direct(1)));
    assert!(!spec.gains.contains_key(&fan.edge(1)));
    assert!(spec.gains.contains_key(&fan.edge(2)));
    spec.validate().expect("no stale entry");

    let fan = Fan::new(&[1.0, 2.0], false);
    let mut spec = GraphSpec::new(fan.topology.clone());
    spec.set_edge_gain(fan.edge(1), EdgeGain::default());
    assert!(spec.disconnect_output(0, out(1)));
    assert!(spec.gains.is_empty());
    spec.validate().expect("no stale entry");

    let mut rig = Rig::new(0.5);
    rig.ed
        .spec_mut()
        .set_edge_gain(output_edge(2), EdgeGain::default());
    rig.ed.remove(key(1));
    assert!(rig.ed.spec_mut().gains.contains_key(&output_edge(2)));
    assert!(!rig.ed.spec_mut().gains.contains_key(&output_edge(1)));
    rig.ed.commit().expect("commits without the stale entry");
}

// ---------------------------------------------------------------------------
// Silence, zero, and the constant flag.
// ---------------------------------------------------------------------------

/// Silent until switched on, then `1.0`.
struct Switch(Arc<AtomicBool>);

impl Node for Switch {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        if self.0.load(Ordering::Relaxed) {
            io.output(0).fill(1.0);
            Status::Modified
        } else {
            Status::Silent
        }
    }
    fn reset(&mut self) {}
}

/// Counts its calls, and those whose input arrived flagged constant; passes
/// the input through.
struct Probe {
    calls: Arc<AtomicU64>,
    constant: Arc<AtomicU64>,
}

impl Node for Probe {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if io.constant().get(0) {
            self.constant.fetch_add(1, Ordering::Relaxed);
        }
        if io.silent().get(0) {
            return Status::Silent;
        }
        io.channel(0).map(|x| x);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// Sources inserted by `source`, summed with `weights` into a probe (key
/// `n + 1`), whose output is the graph's.
struct Mixer {
    ed: Editor,
    exec: Executor,
    calls: Arc<AtomicU64>,
    constant: Arc<AtomicU64>,
}

impl Mixer {
    fn new(sources: Vec<Box<dyn FnOnce(&mut Editor, NodeKey)>>, weights: &[f32]) -> Self {
        let (mut ed, mut exec) = Editor::new(prepare(BLOCK));
        let n = sources.len() as u64;
        for (i, insert) in sources.into_iter().enumerate() {
            insert(&mut ed, key(i as u64 + 1));
        }
        let calls = Arc::new(AtomicU64::new(0));
        let constant = Arc::new(AtomicU64::new(0));
        ed.insert(
            key(n + 1),
            "probe",
            Unforkable(Probe {
                calls: Arc::clone(&calls),
                constant: Arc::clone(&constant),
            }),
        );
        for k in 1..=n {
            ed.spec_mut().topology.connect(at(n + 1, 0), direct(k));
            ed.spec_mut().set_edge_gain(
                port_edge(n + 1, k),
                EdgeGain::new(amp(weights[k as usize - 1])),
            );
        }
        ed.spec_mut().topology.set_outputs(vec![out(n + 1)]);
        ed.commit().expect("commits");
        exec.apply_pending();
        ed.collect();
        Self {
            ed,
            exec,
            calls,
            constant,
        }
    }

    fn block(&mut self) -> Vec<f32> {
        let mut out = vec![0.0f32; BLOCK];
        self.exec
            .process(BLOCK, &Transport::default(), &[], &mut [out.as_mut_slice()]);
        self.ed.collect();
        out
    }

    fn counts(&self) -> (u64, u64) {
        (
            self.calls.load(Ordering::Relaxed),
            self.constant.load(Ordering::Relaxed),
        )
    }
}

fn switch(flag: &Arc<AtomicBool>) -> Box<dyn FnOnce(&mut Editor, NodeKey)> {
    let flag = Arc::clone(flag);
    Box::new(move |ed, k| ed.insert(k, "switch", Unforkable(Switch(flag))))
}

fn constant(v: f32) -> Box<dyn FnOnce(&mut Editor, NodeKey)> {
    Box::new(move |ed, k| ed.insert(k, "dc", Unforkable(TestNode::new(dc(v)))))
}

/// A silent addend is skipped whatever its weight: all-silent sources, with
/// weights of `0.0`, `-3.0` and `1.0`, make a silent sum, and the probe (no
/// tail) is called once and then parked; a live source among them is heard at
/// once, weighted, and the silent ones add nothing.
#[test]
fn silent_addends_are_skipped_whatever_their_weight() {
    let (a, b, c) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let mut m = Mixer::new(vec![switch(&a), switch(&b), switch(&c)], &[0.0, -3.0, 1.0]);
    for _ in 0..4 {
        assert!(m.block().iter().all(|&x| x.to_bits() == 0));
    }
    assert_eq!(m.counts().0, 1, "parked after one call");
    b.store(true, Ordering::Relaxed);
    let heard = m.block();
    assert!(
        heard.iter().all(|&x| x == -3.0),
        "the live lane, weighted: {:?}",
        &heard[..2]
    );
    assert_eq!(m.counts().0, 2, "heard at once");
}

/// A silent addend is skipped but its lane's weight still moves: a fader moved
/// while its source is silent has arrived by the time the source is heard, so
/// the first live block is static at the new weight, not a ramp from the old.
#[test]
fn a_silent_lanes_weight_still_moves() {
    let heard = Arc::new(AtomicBool::new(true));
    let mut m = Mixer::new(vec![switch(&heard)], &[0.5]);
    assert!(m.block().iter().all(|&x| x == 0.5), "live at its weight");
    heard.store(false, Ordering::Relaxed);
    m.ed.set_edge_gain(port_edge(2, 1), amp(2.0))
        .expect("an edge with an entry");
    assert!(m.block().iter().all(|&x| x.to_bits() == 0), "silent");
    heard.store(true, Ordering::Relaxed);
    let got = m.block();
    assert!(
        got.iter().all(|&x| x == 2.0),
        "static at the new weight: {:?}",
        &got[..3]
    );
}

/// A weight of `0.0` is **not** skipped by value: a live source weighted `0.0`
/// renders zeros and keeps the sink called (the sum is not flagged silent, and
/// is constant), and a `NaN` source weighted `0.0` is `NaN` (`0.0 * NaN`),
/// where a skip would make the render depend on whether a gain is zero.
/// Executor and reference (which skips nothing) agree on both.
#[test]
fn a_zero_weight_is_not_skipped_by_value() {
    let mut m = Mixer::new(vec![constant(1.0)], &[0.0]);
    for _ in 0..4 {
        assert!(m.block().iter().all(|&x| x.to_bits() == 0));
    }
    assert_eq!(m.counts(), (4, 4), "called every block, flagged constant");

    let fan = Fan::new(&[f32::NAN, 0.5], false);
    let graph = fan.spec(&[0.0, 1.0]);
    for ch in render_both(&graph, &fan.kinds, 2) {
        assert!(ch.iter().all(|x| x.is_nan()), "0.0 * NaN is NaN");
    }
}

/// A weighted sum is constant when its live addends are and none is ramping:
/// a constant times a ramp is not constant. One block static (constant), one
/// ramping (not), one static again (constant).
#[test]
fn a_ramping_sum_is_not_constant() {
    let mut m = Mixer::new(vec![constant(0.5)], &[0.5]);
    m.block();
    assert_eq!(m.counts(), (1, 1), "static: constant");
    m.ed.set_edge_gain(port_edge(2, 1), amp(1.0))
        .expect("an edge with an entry");
    m.block();
    assert_eq!(m.counts(), (2, 1), "ramping: not constant");
    m.block();
    assert_eq!(m.counts(), (3, 2), "static again: constant");
}

// ---------------------------------------------------------------------------
// The snapshot, on the executor.
// ---------------------------------------------------------------------------

/// The cell is read once per block on the executor too: every block's ramp
/// ends on the weight a store before the block left, and a store between two
/// blocks lands whole on the next. (The mid-block case, a store while ops
/// run, is pinned on `SumLane` in `gain.rs`'s tests: an integration test
/// cannot place a store between two ops without a thread race.)
#[test]
fn every_block_reads_the_cell_at_its_start() {
    let mut rig = Rig::new(1.0);
    rig.block();
    let mut prev = 1.0f32;
    for next in [0.0f32, 2.0, -1.0, 0.5] {
        rig.ed.set_edge_gain(output_edge(1), amp(next)).expect("ok");
        let got = rig.block();
        for (i, (&g, w)) in got.iter().zip(ramp(prev, next, BLOCK)).enumerate() {
            assert_eq!(g.to_bits(), ((0.0 + w * 1.0) + 2.0).to_bits(), "frame {i}");
        }
        prev = next;
    }
}

// ---------------------------------------------------------------------------
// A fork carries the entries.
// ---------------------------------------------------------------------------

/// A fork of the master sums as the live graph does: the entries of the edges
/// it keeps ride along, each lane starting at its base (a ramp in flight is
/// not forked).
#[test]
fn a_fork_carries_the_edge_gains() {
    #[derive(Clone)]
    struct Level(f32);
    impl Node for Level {
        fn shape(&self) -> Shape {
            Shape::source_unassigned(ChannelLayout::MONO)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
            io.output(0).fill(self.0);
            Status::Modified
        }
        fn reset(&mut self) {}
    }
    let (mut ed, exec) = Editor::new(prepare(BLOCK));
    ed.insert(key(1), "a", ForkByClone(Level(1.0)));
    ed.insert(key(2), "b", ForkByClone(Level(2.0)));
    ed.spec_mut().topology.outputs = vec![vec![out(1), out(2)]];
    ed.spec_mut()
        .set_edge_gain(output_edge(1), EdgeGain::new(amp(0.5)));
    ed.spec_mut()
        .set_edge_gain(output_edge(2), EdgeGain::new(amp(-2.0)));
    ed.commit().expect("commits");
    let (fork_ed, fork_exec) = ed
        .fork(ForkTarget::Master, ForkMode::Live, prepare(BLOCK))
        .expect("forkable");
    let got = Renderer::new(fork_ed, fork_exec).render(2 * BLOCK);
    assert_eq!(got.len(), 1);
    assert!(
        got[0].iter().all(|&x| x == (0.0 + 0.5 * 1.0) + -2.0 * 2.0),
        "{:?}",
        &got[0][..4]
    );
    drop((ed, exec));
}

/// The base a lane's knob wrote is the one a fork starts at: the knob is the
/// gain's own cell (`Editor::edge_gain_cell`), the spec's copy is behind it,
/// and `Editor::edge_gain` and the fork read the cell.
#[test]
fn a_fork_starts_at_the_base_the_knob_wrote() {
    #[derive(Clone)]
    struct Level(f32);
    impl Node for Level {
        fn shape(&self) -> Shape {
            Shape::source_unassigned(ChannelLayout::MONO)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
            io.output(0).fill(self.0);
            Status::Modified
        }
        fn reset(&mut self) {}
    }
    let (mut ed, exec) = Editor::new(prepare(BLOCK));
    ed.insert(key(1), "a", ForkByClone(Level(1.0)));
    ed.spec_mut().topology.outputs = vec![vec![out(1)]];
    ed.spec_mut()
        .set_edge_gain(output_edge(1), EdgeGain::new(amp(0.5)));
    ed.commit().expect("commits");
    let knob = ed.edge_gain_cell(output_edge(1)).expect("a cell once sent");
    knob.store(0.25, std::sync::atomic::Ordering::Release);
    assert_eq!(ed.edge_gain(output_edge(1)), Some(amp(0.25)), "the cell's");
    let (fork_ed, fork_exec) = ed
        .fork(ForkTarget::Master, ForkMode::Live, prepare(BLOCK))
        .expect("forkable");
    let got = Renderer::new(fork_ed, fork_exec).render(2 * BLOCK);
    assert!(got[0].iter().all(|&x| x == 0.0 + 0.25 * 1.0));
    drop((ed, exec));
}

/// The commit refuses a stale entry rather than weighting nothing.
#[test]
fn a_commit_refuses_an_entry_without_its_edge() {
    let mut rig = Rig::new(0.5);
    rig.ed
        .spec_mut()
        .set_edge_gain(output_edge(7), EdgeGain::default());
    assert!(matches!(
        rig.ed.commit(),
        Err(CommitError::Invalid(e)) if matches!(e[..], [GraphInvalid::GainWithoutEdge { .. }])
    ));
}
