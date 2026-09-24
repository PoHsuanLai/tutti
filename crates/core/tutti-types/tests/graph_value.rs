//! The `Topology` value: validation, folds, and structural identity.
//!
//! Every test here is a plain unit test with no `Net`, no `World`, no device and
//! no audio callback — which is the point of the layer. The equivalent
//! assertions today need `bevy_tutti`'s `App` plus a committed graph, which is
//! why PDC is tested on a fixture and never on a production graph.
//!
//! The `Net` half — that a compiled graph agrees with the value it was compiled
//! from — lives in `tutti-core/tests/topology_compile.rs`, because it needs the
//! runtime this crate deliberately cannot name.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use tutti_types::graph::{
    Edge, FeedbackFrom, InPort, Invalid, NodeKey, NodeSpec, OutPort, ParamValue, Source, Topology,
};
use tutti_types::latency::{self, LatencyGraph};
use tutti_types::tail::graph_tail;
use tutti_types::{ChannelLayout, Samples, Tail};

const A: NodeKey = NodeKey(1);
const B: NodeKey = NodeKey(2);
const C: NodeKey = NodeKey(3);
const D: NodeKey = NodeKey(4);

fn spec(kind: &str, ins: u16, outs: u16) -> NodeSpec {
    NodeSpec::new(
        kind,
        ChannelLayout::from_count(ins),
        ChannelLayout::from_count(outs),
    )
}

fn at(node: NodeKey, port: u16) -> InPort {
    InPort { node, port }
}

fn out(node: NodeKey, port: u16) -> OutPort {
    OutPort { node, port }
}

/// `dc -> gain -> output 0`.
fn chain() -> Topology {
    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes.insert(B, spec("gain", 1, 1));
    t.edges
        .insert(at(B, 0), Edge::Direct(Source::Node(out(A, 0))));
    t.outputs = vec![Source::Node(out(B, 0))];
    t
}

/// The `latency.rs` module-doc diagram, as a value: two sources merge into a
/// mixer, one of them through a 512-frame lookahead limiter.
fn unequal_latency_diamond() -> Topology {
    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes.insert(D, spec("dc", 0, 1));
    t.nodes.insert(
        B,
        spec("lookahead", 1, 1)
            .with_latency(Samples(512))
            .with_tail(Tail::Finite(Samples(64))),
    );
    t.nodes.insert(C, spec("mix", 2, 1));
    t.edges
        .insert(at(B, 0), Edge::Direct(Source::Node(out(A, 0))));
    t.edges
        .insert(at(C, 0), Edge::Direct(Source::Node(out(B, 0))));
    t.edges
        .insert(at(C, 1), Edge::Direct(Source::Node(out(D, 0))));
    t.outputs = vec![Source::Node(out(C, 0))];
    t
}

fn hash_of<T: Hash>(v: &T) -> u64 {
    let mut h = DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

/// `latency::plan` and `tail::graph_tail` — the two best-tested files in the
/// engine — answer questions about a `Topology` with **no changes to either**,
/// because `Topology` implements the traits they were already generic over.
///
/// The graph is the exact diagram in `latency.rs`'s module doc, so the answer
/// must be the same 512, driven by a value a test can write down.
///
/// Mutation: make `LatencyGraph::inputs` yield the sink's own key instead of the
/// source's → the merge no longer sees the 512 and `total()` is 0 → fails.
#[test]
fn latency_and_tail_are_folds_over_the_value() {
    let t = unequal_latency_diamond();
    t.validate().expect("well formed");

    let comp = latency::plan(&t);
    assert_eq!(comp.total(), Samples(512), "graph latency is the slow path");
    // The un-delayed branch is the one that needs pre-roll; the compensation is
    // reported per output channel, and there is one.
    assert_eq!(comp.channels(), [Samples(0)]);

    let tail = graph_tail(&t);
    assert_eq!(tail.samples(), Some(Samples(64)));
    assert_eq!(tail.unknown_nodes(), 0);

    // The trait view agrees with the structure: the mixer's port order is the
    // authored one, holes and all.
    let preds: Vec<Option<NodeKey>> = t.inputs(C).collect();
    assert_eq!(preds, vec![Some(B), Some(D)]);
}

/// A node whose tail was never declared is counted, not collapsed to zero —
/// `Tail::Unknown` is the `NodeSpec` default a catalog has not filled in.
///
/// Mutation: default `TailGraph::tail` to `Tail::None` instead of `Unknown` →
/// `unknown_nodes()` is 0 → fails.
#[test]
fn an_undeclared_tail_is_counted_as_unknown() {
    let mut t = chain();
    t.nodes.get_mut(&B).unwrap().tail = Tail::Unknown;

    let tail = graph_tail(&t);
    assert_eq!(tail.unknown_nodes(), 1);
    assert_eq!(tail.samples(), None, "an unknown node has no finite answer");
}

/// `validate` is the type-index replacement: it catches what a width-indexed
/// node type would have, and reports **every** fault at once rather than the
/// first — an author fixing a graph should not have to bisect.
///
/// Mutation: `?`-style short-circuit after the first fault (return `Err(errs)`
/// inside the edge loop) → the assertion that all five kinds are present fails.
#[test]
fn validate_reports_every_fault_at_once() {
    let mut t = Topology::default();
    // A declares 1 in / 1 out; B declares 2 in / 1 out.
    t.nodes.insert(A, spec("gain", 1, 1));
    t.nodes.insert(B, spec("mix", 2, 1));
    t.inputs = ChannelLayout::MONO;

    // (1) an edge from a port A does not have
    t.edges
        .insert(at(B, 0), Edge::Direct(Source::Node(out(A, 7))));
    // (2) an edge into a port B does not have
    t.edges
        .insert(at(B, 9), Edge::Direct(Source::Node(out(A, 0))));
    // (3) an edge naming a node that is not in the map
    t.edges
        .insert(at(A, 0), Edge::Direct(Source::Node(out(C, 0))));
    // (4) a global-input edge past the declared input width
    t.edges.insert(at(B, 1), Edge::Direct(Source::Global(5)));
    // (5) an output channel from a port that does not exist
    t.outputs = vec![Source::Node(out(B, 3))];

    let errs = t.validate().expect_err("five faults");

    assert!(
        errs.iter()
            .any(|e| matches!(e, Invalid::SourcePortOutOfRange { from, .. } if from.port == 7)),
        "{errs:?}"
    );
    assert!(
        errs.iter()
            .any(|e| matches!(e, Invalid::SinkPortOutOfRange { at, .. } if at.port == 9)),
        "{errs:?}"
    );
    assert!(
        errs.iter()
            .any(|e| matches!(e, Invalid::UnknownNode { missing, .. } if *missing == C)),
        "{errs:?}"
    );
    assert!(
        errs.iter()
            .any(|e| matches!(e, Invalid::GlobalInputOutOfRange { channel, .. } if *channel == 5)),
        "{errs:?}"
    );
    assert!(
        errs.iter()
            .any(|e| matches!(e, Invalid::OutputOutOfRange { channel, .. } if *channel == 0)),
        "{errs:?}"
    );
}

/// An unbroken cycle is rejected; the same cycle with the returning edge
/// declared as feedback validates, because the one-block delay rides on the edge
/// *kind* and so cannot be forgotten.
///
/// Mutation: include `Edge::Feedback` in `direct_preds` → the feedback graph is
/// reported as a cycle → the `expect("feedback breaks the cycle")` fails.
#[test]
fn a_cycle_is_rejected_unless_feedback_breaks_it() {
    let mut t = Topology::default();
    t.nodes.insert(A, spec("gain", 1, 1));
    t.nodes.insert(B, spec("gain", 1, 1));
    t.edges
        .insert(at(B, 0), Edge::Direct(Source::Node(out(A, 0))));
    t.edges
        .insert(at(A, 0), Edge::Direct(Source::Node(out(B, 0))));
    t.outputs = vec![Source::Node(out(B, 0))];

    let errs = t.validate().expect_err("a -> b -> a");
    let cycle = errs
        .iter()
        .find_map(|e| match e {
            Invalid::Cycle { involving } => Some(involving),
            _ => None,
        })
        .expect("a Cycle fault");
    assert_eq!(cycle, &vec![A, B], "both nodes are named");

    let mut broken = t.clone();
    broken
        .edges
        .insert(at(A, 0), Edge::Feedback(FeedbackFrom { from: out(B, 0) }));
    let valid = broken.validate().expect("feedback breaks the cycle");
    assert_eq!(
        valid.get().topo_order().expect("acyclic once cut"),
        vec![A, B]
    );

    // The FOLDS must agree with `validate` about what a feedback edge is, or the
    // two disagree about the graph's shape. A hole, not a predecessor: the value
    // arrives from last block, so it contributes no latency along this block's
    // path — and treating it as live puts the walk back in the cycle it was just
    // told to cut, which `plan` resolves by silently appending the cyclic nodes
    // in arbitrary order.
    let preds: Vec<Option<NodeKey>> = valid.get().inputs(A).collect();
    assert_eq!(preds, vec![None]);

    // With a real latency inside the loop, the fold still terminates on the
    // acyclic reading and counts the node once.
    let mut lat = broken.clone();
    lat.nodes.get_mut(&A).unwrap().latency = Samples(128);
    let lat = lat.validate().expect("still acyclic");
    assert_eq!(latency::plan(lat.get()).total(), Samples(128));
}

/// An unconnected declared port is reported and does **not** block validity: a
/// half-built graph is renderable and reads silence there, which is what a graph
/// mid-edit looks like.
///
/// Mutation: make `Invalid::is_fatal` return `true` unconditionally → the
/// `expect("unconnected is not fatal")` fails.
#[test]
fn unconnected_is_reported_but_not_fatal() {
    let mut t = chain();
    t.nodes.insert(C, spec("mix", 2, 1));

    let unconnected: Vec<InPort> = t.unconnected().collect();
    assert_eq!(unconnected, vec![at(C, 0), at(C, 1)]);

    let valid = t.validate().expect("unconnected is not fatal");
    assert_eq!(valid.get(), &t, "the value is carried through unchanged");
    assert!(!Invalid::Unconnected { at: at(C, 0) }.is_fatal());
}

/// Two topologies built in **different insertion orders** are equal and hash
/// equal. This is what makes the value usable as a cache key and as the whole of
/// a change check — `Net::revision` is monotone but is not a function of the
/// graph, so it can order two states and cannot identify one.
///
/// Mutation: swap `Topology::nodes` to a `HashMap` → the derived `Hash` stops
/// being order-independent and the hash assertion fails (intermittently, which
/// is exactly why the field is a `BTreeMap`).
#[test]
fn insertion_order_does_not_change_the_value() {
    let forward = unequal_latency_diamond();

    // The same graph, every insertion reversed.
    let mut backward = Topology {
        outputs: vec![Source::Node(out(C, 0))],
        ..Topology::default()
    };
    backward
        .edges
        .insert(at(C, 1), Edge::Direct(Source::Node(out(D, 0))));
    backward
        .edges
        .insert(at(C, 0), Edge::Direct(Source::Node(out(B, 0))));
    backward
        .edges
        .insert(at(B, 0), Edge::Direct(Source::Node(out(A, 0))));
    backward.nodes.insert(C, spec("mix", 2, 1));
    backward.nodes.insert(
        B,
        spec("lookahead", 1, 1)
            .with_latency(Samples(512))
            .with_tail(Tail::Finite(Samples(64))),
    );
    backward.nodes.insert(D, spec("dc", 0, 1));
    backward.nodes.insert(A, spec("dc", 0, 1));

    assert_eq!(forward, backward);
    assert_eq!(hash_of(&forward), hash_of(&backward));
    assert_eq!(forward.topo_order(), backward.topo_order());
    assert_eq!(
        latency::plan(&forward).total(),
        latency::plan(&backward).total()
    );
}

/// A param differing by one ULP is a **different graph**. Structural, not
/// numeric: this relation decides whether a node survives a recompile, and
/// collapsing near-equal values would keep a unit built from the old number.
///
/// Mutation: `#[derive(PartialEq)]` on `ParamValue` instead of the hand-written
/// bit-pattern impl → `0.0 == -0.0` and `NaN != NaN` → the last two assertions
/// fail (and `Hash`/`Eq` silently disagree, which is what the derive costs).
#[test]
fn param_equality_is_structural() {
    let base = spec("gain", 1, 1).with_param("gain", ParamValue::Scalar(0.5));
    let same = spec("gain", 1, 1).with_param("gain", ParamValue::Scalar(0.5));
    let nudged = spec("gain", 1, 1).with_param(
        "gain",
        ParamValue::Scalar(f32::from_bits(0.5f32.to_bits() + 1)),
    );

    assert_eq!(base, same);
    assert_eq!(hash_of(&base), hash_of(&same));
    assert_ne!(base, nudged);

    let zero = spec("gain", 1, 1).with_param("gain", ParamValue::Scalar(0.0));
    let neg_zero = spec("gain", 1, 1).with_param("gain", ParamValue::Scalar(-0.0));
    assert_ne!(zero, neg_zero, "two spellings of zero are two graphs");
    assert_ne!(
        hash_of(&zero),
        hash_of(&neg_zero),
        "and Hash agrees with Eq, which is what makes the map usable"
    );

    // `Eq` must be reflexive. Derived float equality is not, and a spec that is
    // unequal to itself makes a `Topology` unfindable in its own map.
    let nan = spec("gain", 1, 1).with_param("gain", ParamValue::Scalar(f32::NAN));
    assert_eq!(nan, nan.clone());
}

/// The default topology has **no** global inputs.
///
/// Not a formality: when `ChannelLayout::default()` was `STEREO`, a *derived*
/// `Default` on `Topology` silently gave every master graph two input channels
/// nobody declared — and a `Source::Global(1)` typo then resolved against them
/// instead of being rejected as out of range. `ChannelLayout` has no `Default`
/// any more, so that derive no longer compiles; this pins the hand-written
/// impl's *choice* of width, which the compiler cannot.
///
/// Mutation: `inputs: ChannelLayout::STEREO` in the hand-written impl → the
/// out-of-range edge validates → both assertions fail.
#[test]
fn the_default_topology_has_no_global_inputs() {
    let t = Topology::default();
    assert_eq!(t.inputs, ChannelLayout::EMPTY);

    let mut t = chain();
    t.edges.insert(at(B, 0), Edge::Direct(Source::Global(1)));
    let errs = t.validate().expect_err("no global input to name");
    assert!(
        errs.iter()
            .any(|e| matches!(e, Invalid::GlobalInputOutOfRange { channel, .. } if *channel == 1)),
        "{errs:?}"
    );
}

/// `topo_order` is a *function* of the value, not merely one valid answer: the
/// queue is seeded from a `BTreeMap`, so the exact vector is assertable.
///
/// Mutation: seed the Kahn queue from a `HashMap` → the order varies run to run
/// → this fails intermittently.
#[test]
fn topo_order_is_deterministic() {
    let t = unequal_latency_diamond();
    let order = t.topo_order().expect("acyclic");
    // Depth-first from the lowest zero-in-degree key: A, then A's newly-ready
    // dependent B, then the other root D, then the merge C. Asserting the exact
    // vector — not merely `is_sorted_topologically` — is the point: any valid
    // order would pass a weaker check, and then a nondeterministic seed would
    // too.
    assert_eq!(order, vec![A, B, D, C]);

    // And the same value gives the same answer every time, not merely a valid
    // one — ten runs, one vector.
    for _ in 0..10 {
        assert_eq!(t.clone().topo_order().expect("acyclic"), order);
    }
}
