//! `Topology` → [`compile`] → `Net`, and the proof that the two agree.
//!
//! Three claims, in order of how much they buy:
//!
//! 1. Every edge in the value lands as the same edge in the `Net` — read back
//!    through `Net::source` / `output_source`, which is the only view the engine
//!    offers, over a dozen shapes.
//! 2. `latency::plan` and `tail::graph_tail` give the **same answer** over the
//!    value as over the graph compiled from it. That is the whole proof: the two
//!    best-tested files in the engine now answer questions about a value a unit
//!    test can write down, and their answer is the runtime's.
//! 3. `Engine` renders a compiled graph to the samples the value predicts —
//!    `tutti-core`'s first end-to-end engine test, which the layer is what makes
//!    writable at all (it needs no `World` and no device).

use std::any::Any;
use std::sync::Arc;

use parking_lot::Mutex;
use tutti_core::dsp::{Net, Source as NetSource};
use tutti_core::topology::{compile, Catalog, CompileError, Compiled};
use tutti_core::{
    graph::{
        Edge, FeedbackFrom, InPort, NodeKey, NodeSpec, OutPort, ParamValue, Source, Topology, Valid,
    },
    latency, tail, ChannelLayout, Engine, InterleavedMut, MotionEvent, SampleRate, Samples, Tail,
    Transport, TransportClock,
};
use tutti_core::{AudioUnit, BufferMut, BufferRef, Setting, Signal, SignalFrame};

const RATE: SampleRate = SampleRate(48_000.0);

// ---------------------------------------------------------------------------
// A test catalog.
// ---------------------------------------------------------------------------

/// How a [`TestUnit`] turns its inputs into its outputs.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Behaviour {
    /// Emit `value` on every output. No inputs.
    Dc(f32),
    /// Multiply input `i` by `gain` onto output `i`.
    Gain(f32),
    /// Sum every input onto every output.
    Sum,
    /// Copy input `i` to output `i`, wrapping when the widths differ.
    Fan,
}

/// One unit covering every shape the catalog builds.
///
/// Hand-rolled rather than assembled from the fundsp DSL because two of its
/// methods are the point of the test: `route` is where `AudioUnit::latency` is
/// *derived* from, and `tail` is reported directly. A DSL node reports the
/// latency of whatever it is made of and `Tail::Unknown`, so a compiled graph
/// built from one could not be compared against a spec that declares either.
#[derive(Clone)]
struct TestUnit {
    inputs: usize,
    outputs: usize,
    latency: Samples,
    tail: Tail,
    behaviour: Behaviour,
}

impl TestUnit {
    fn value(&self, input: &[f32], channel: usize) -> f32 {
        match self.behaviour {
            Behaviour::Dc(v) => v,
            Behaviour::Gain(g) => input.get(channel).copied().unwrap_or(0.0) * g,
            Behaviour::Sum => input.iter().sum(),
            Behaviour::Fan => {
                if input.is_empty() {
                    0.0
                } else {
                    input[channel % input.len()]
                }
            }
        }
    }
}

impl AudioUnit for TestUnit {
    fn reset(&mut self) {}

    fn set_sample_rate(&mut self, _sample_rate: tutti_core::SampleRate) {}

    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        for (channel, slot) in output.iter_mut().enumerate() {
            *slot = self.value(input, channel);
        }
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        let mut frame = vec![0.0f32; self.inputs];
        for i in 0..size {
            for (channel, slot) in frame.iter_mut().enumerate() {
                *slot = input.at_f32(channel, i);
            }
            for channel in 0..self.outputs {
                output.set_f32(channel, i, self.value(&frame, channel));
            }
        }
    }

    fn set(&mut self, _setting: Setting) {}

    fn inputs(&self) -> usize {
        self.inputs
    }

    fn outputs(&self) -> usize {
        self.outputs
    }

    /// Reports `latency` on every output, which is what `AudioUnit::latency`
    /// reads back — the *only* route by which a spec's declared latency can
    /// reach `Net`'s `LatencyGraph` impl.
    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut out = SignalFrame::new(self.outputs);
        for channel in 0..self.outputs {
            out.set(channel, Signal::Latency(self.latency.get() as f64));
        }
        out
    }

    fn tail(&mut self) -> Tail {
        self.tail
    }

    fn get_id(&self) -> u64 {
        0x_0000_0054_5553_5401 // "TUST"
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

/// Builds one [`TestUnit`] per kind, at the spec's declared widths.
struct TestCatalog;

impl Catalog for TestCatalog {
    fn build(&self, spec: &NodeSpec, _sample_rate: SampleRate) -> Option<Box<dyn AudioUnit>> {
        let behaviour = match spec.kind.as_str() {
            "dc" => Behaviour::Dc(spec.scalar("value").unwrap_or(1.0)),
            "gain" => Behaviour::Gain(spec.scalar("gain").unwrap_or(1.0)),
            "sum" => Behaviour::Sum,
            "fan" => Behaviour::Fan,
            // A width the catalog gets deliberately wrong, so the mismatch check
            // has something to catch. Named, not silent.
            "liar" => {
                return Some(Box::new(TestUnit {
                    inputs: 1,
                    outputs: 1,
                    latency: Samples::ZERO,
                    tail: Tail::None,
                    behaviour: Behaviour::Fan,
                }))
            }
            _ => return None,
        };
        Some(Box::new(TestUnit {
            inputs: spec.inputs.count() as usize,
            outputs: spec.outputs.count() as usize,
            latency: spec.latency,
            tail: spec.tail,
            behaviour,
        }))
    }
}

// ---------------------------------------------------------------------------
// Builders.
// ---------------------------------------------------------------------------

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
    // Every test node states its ring-out, so a `Tail::Unknown` in a result is
    // a finding rather than the default leaking through.
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

/// The dozen shapes, each named for the structure it exercises.
fn shapes() -> Vec<(&'static str, Topology)> {
    let mut all = Vec::new();

    // 1. A chain: dc → gain → out.
    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes.insert(B, spec("gain", 1, 1));
    edge(&mut t, at(B, 0), out(A, 0));
    t.outputs = vec![Source::Node(out(B, 0))];
    all.push(("chain", t));

    // 2. Fan-out: one source into two sinks.
    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes.insert(B, spec("gain", 1, 1));
    t.nodes.insert(C, spec("gain", 1, 1));
    edge(&mut t, at(B, 0), out(A, 0));
    edge(&mut t, at(C, 0), out(A, 0));
    t.outputs = vec![Source::Node(out(B, 0)), Source::Node(out(C, 0))];
    all.push(("fan_out", t));

    // 3. Fan-in through a summing NODE — the only way fan-in is expressible.
    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes.insert(B, spec("dc", 0, 1));
    t.nodes.insert(C, spec("sum", 2, 1));
    edge(&mut t, at(C, 0), out(A, 0));
    edge(&mut t, at(C, 1), out(B, 0));
    t.outputs = vec![Source::Node(out(C, 0))];
    all.push(("mix_fan_in", t));

    // 4. A diamond: one source splits and re-merges.
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

    // 5. The SAME diamond with unequal latencies — the PDC case, and the exact
    //    diagram in `latency.rs`'s module doc.
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

    // 6. Two latencies on ONE path, so the arrival is a sum rather than a max.
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

    // 7. A multi-output node: one stereo source drives both channels.
    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 2));
    t.outputs = vec![Source::Node(out(A, 0)), Source::Node(out(A, 1))];
    all.push(("multi_output", t));

    // 8. Global input passthrough: the graph has inputs, and an edge names one.
    let mut t = Topology {
        inputs: ChannelLayout::STEREO,
        ..Topology::default()
    };
    t.nodes.insert(A, spec("fan", 2, 2));
    t.edges.insert(at(A, 0), Edge::Direct(Source::Global(0)));
    t.edges.insert(at(A, 1), Edge::Direct(Source::Global(1)));
    t.outputs = vec![Source::Node(out(A, 0)), Source::Node(out(A, 1))];
    all.push(("global_input", t));

    // 9. A zero-sourced port: silence declared on purpose, distinct from absent.
    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes.insert(B, spec("sum", 2, 1));
    edge(&mut t, at(B, 0), out(A, 0));
    t.edges.insert(at(B, 1), Edge::Direct(Source::Zero));
    t.outputs = vec![Source::Node(out(B, 0))];
    all.push(("zero_sourced_port", t));

    // 10. A zero output channel beside a live one.
    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.outputs = vec![Source::Node(out(A, 0)), Source::Zero];
    all.push(("silent_output_channel", t));

    // 11. Wide: a 6-channel node straight to a 6-channel master.
    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 6));
    t.nodes.insert(B, spec("fan", 6, 6));
    for port in 0..6 {
        edge(&mut t, at(B, port), out(A, port));
    }
    t.outputs = (0..6).map(|p| Source::Node(out(B, p))).collect();
    all.push(("six_channel", t));

    // 12. Ring-out: a node that keeps sounding after its input stops, behind one
    //     that does not, so `graph_tail` has a path to accumulate along.
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

    // 13. An isolated node nothing reads — the graph is still well formed, and
    //     the plans must agree about a node that contributes to no output.
    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes
        .insert(B, spec("gain", 1, 1).with_latency(Samples(999)));
    edge(&mut t, at(B, 0), out(A, 0));
    t.outputs = vec![Source::Node(out(A, 0))];
    all.push(("orphan_branch", t));

    all
}

fn compiled(t: &Topology) -> (Valid, Compiled) {
    let valid = t.validate().expect("shape is well formed");
    let built = compile(&valid, &TestCatalog, RATE).expect("catalog builds every kind");
    (valid, built)
}

/// The compile error, or a panic. `Net` is not `Debug`, so `expect_err` cannot
/// be used directly on a `Result<Net, _>`; discarding the `Ok` side here keeps
/// the error assertions readable rather than pushing a `match` into each test.
fn compile_err(t: &Topology) -> CompileError {
    let valid = t.validate().expect("structurally fine");
    match compile(&valid, &TestCatalog, RATE) {
        Ok(_) => panic!("expected a compile error"),
        Err(e) => e,
    }
}

// ---------------------------------------------------------------------------
// 1. Every edge lands.
// ---------------------------------------------------------------------------

/// Compile a dozen shapes and read every edge back off the `Net`. The `Net`'s
/// own `source` / `output_source` is the only view the engine offers, so this is
/// the strongest available statement that the value and the runtime describe the
/// same graph.
///
/// Mutation: swap `Source::Node(p) => NetSource::Local(id, p.port)` to
/// `Local(id, 0)` in `lower` → the multi-output and six-channel shapes read back
/// channel 0 everywhere → fails.
#[test]
fn every_edge_in_the_value_lands_in_the_net() {
    // A loop over an empty list passes vacuously; this is what stops that.
    assert_eq!(shapes().len(), 13, "the shape list is the coverage");

    for (name, t) in shapes() {
        let (_, built) = compiled(&t);
        let net = &built.net;

        // The node set is exactly the value's, at exactly the value's widths.
        assert_eq!(net.ids().count(), t.nodes.len(), "{name}: node count");
        assert_eq!(built.ids.len(), t.nodes.len(), "{name}: id map covers it");

        // `Net` mints its own ids from a global counter and yields them in hash
        // order, so the value's keys are matched back through the map `compile`
        // returns — which is the reason it returns one.
        let id_of = |k: NodeKey| built.ids.get(&k).copied();

        for (key, spec) in &t.nodes {
            let id = id_of(*key).expect("every key has an id");
            assert_eq!(
                net.inputs_in(id),
                spec.inputs.count() as usize,
                "{name}: node {} input width",
                key.0
            );
            assert_eq!(
                net.outputs_in(id),
                spec.outputs.count() as usize,
                "{name}: node {} output width",
                key.0
            );
        }

        for (at, e) in &t.edges {
            let Edge::Direct(source) = e else {
                unreachable!("no shape declares feedback")
            };
            let sink = id_of(at.node).expect("sink has an id");
            let want = match *source {
                Source::Node(p) => NetSource::Local(id_of(p.node).unwrap(), p.port as usize),
                Source::Global(ch) => NetSource::Global(ch as usize),
                Source::Zero => NetSource::Zero,
            };
            assert_eq!(
                net.source(sink, at.port as usize),
                want,
                "{name}: edge into node {} port {}",
                at.node.0,
                at.port
            );
        }

        for (channel, source) in t.outputs.iter().enumerate() {
            let want = match *source {
                Source::Node(p) => NetSource::Local(id_of(p.node).unwrap(), p.port as usize),
                Source::Global(ch) => NetSource::Global(ch as usize),
                Source::Zero => NetSource::Zero,
            };
            assert_eq!(
                net.output_source(channel),
                want,
                "{name}: output channel {channel}"
            );
        }
    }
}

/// A port the value leaves out is `Zero` in the `Net` — the same reading a
/// half-built graph gets, and the reason `Unconnected` is not fatal.
///
/// Mutation: have `compile` skip `set_output_source` for `Source::Zero` → still
/// passes (`Net`'s default is `Zero`), which is why the assertion below is on an
/// *undeclared node port*, whose default is also `Zero` but which no shape
/// writes.
#[test]
fn an_undeclared_port_reads_zero() {
    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes.insert(B, spec("sum", 2, 1));
    edge(&mut t, at(B, 0), out(A, 0));
    t.outputs = vec![Source::Node(out(B, 0))];

    assert_eq!(t.unconnected().collect::<Vec<_>>(), vec![at(B, 1)]);

    let (_, built) = compiled(&t);
    assert_eq!(built.net.source(built.ids[&B], 1), NetSource::Zero);
}

// ---------------------------------------------------------------------------
// 2. THE PROOF: the folds agree.
// ---------------------------------------------------------------------------

/// `latency::plan` over the **value** equals `latency::plan` over the `Net`
/// compiled from it, for every shape. Likewise `tail::graph_tail`.
///
/// This is the claim the whole layer rests on. Both sides run the *same*
/// algorithm — `tutti_types::latency::plan`, unchanged — over two different
/// `LatencyGraph` impls: `Topology`'s (three lines, added by this PR) and
/// `Net`'s (which reads back through `route()`, cloning each node to probe it).
/// Their agreeing is what makes a `Topology` answer for the runtime, so a caller
/// can plan latency without a device.
///
/// Mutation: make `Topology::inputs` yield `0..outputs.count()` instead of
/// `0..inputs.count()` → `pdc_diamond` and `serial_latency` disagree → fails.
/// Mutation: make `TailGraph::tail` for `Topology` return `Tail::None` for a
/// missing node instead of `Unknown` → `tail_chain` still passes but
/// `orphan_branch` diverges → fails.
#[test]
fn the_plans_agree_over_the_value_and_the_compiled_net() {
    assert_eq!(shapes().len(), 13, "the shape list is the coverage");

    for (name, t) in shapes() {
        let (_, built) = compiled(&t);
        let net = &built.net;

        let from_value = latency::plan(&t);
        let from_net = latency::plan(net);
        assert_eq!(
            from_value.total(),
            from_net.total(),
            "{name}: total latency"
        );
        assert_eq!(
            from_value.channels(),
            from_net.channels(),
            "{name}: per-channel compensation"
        );

        let tail_value = tail::graph_tail(&t);
        let tail_net = tail::graph_tail(net);
        assert_eq!(
            tail_value.samples(),
            tail_net.samples(),
            "{name}: graph tail"
        );
        assert_eq!(
            tail_value.unknown_nodes(),
            tail_net.unknown_nodes(),
            "{name}: unknown-tail node count"
        );
        assert_eq!(
            tail_value.is_unbounded(),
            tail_net.is_unbounded(),
            "{name}: unbounded"
        );
    }
}

/// The agreement above is not vacuous: at least one shape has a *non-zero*
/// answer on both sides, so a bug that made both planners return the default
/// would be caught.
///
/// Mutation: return `Compensation::default()` from `plan` → this fails while the
/// equality test above would still pass.
#[test]
fn the_agreed_plans_are_not_all_zero() {
    let shapes = shapes();
    let (_, pdc) = shapes
        .iter()
        .find(|(n, _)| *n == "pdc_diamond")
        .expect("shape present");
    let (_, built) = compiled(pdc);
    assert_eq!(latency::plan(pdc).total(), Samples(512));
    assert_eq!(latency::plan(&built.net).total(), Samples(512));

    let (_, tails) = shapes
        .iter()
        .find(|(n, _)| *n == "tail_chain")
        .expect("shape present");
    let (_, built) = compiled(tails);
    assert_eq!(tail::graph_tail(tails).samples(), Some(Samples(2_304)));
    assert_eq!(tail::graph_tail(&built.net).samples(), Some(Samples(2_304)));
}

// ---------------------------------------------------------------------------
// 3. Compile errors name the node.
// ---------------------------------------------------------------------------

/// A kind no catalog claims is an error on the control thread naming the node,
/// not a silent gap discovered by listening.
///
/// Mutation: `unwrap_or_else(|| Box::new(silence))` in place of the
/// `ok_or_else` → no error → fails.
#[test]
fn an_unknown_kind_is_an_error_naming_the_node() {
    let mut t = Topology::default();
    t.nodes.insert(A, spec("no-such-kind", 0, 1));
    t.outputs = vec![Source::Node(out(A, 0))];

    let err = compile_err(&t);
    assert_eq!(
        err,
        CompileError::UnknownKind {
            node: A,
            kind: "no-such-kind".to_string()
        }
    );
    assert!(err.to_string().contains("no-such-kind"));
}

/// A catalog whose unit disagrees with the spec's width is caught before the
/// edges are written. Without the check the mismatch reaches `Net::set_source`,
/// which `assert!`s — a panic naming a port, from inside a graph rebuild.
///
/// Mutation: delete the `check_width` call → the compile panics instead of
/// returning → the test fails (as a panic, which is still a failure, but the
/// error-shape assertion is what makes the intent legible).
#[test]
fn a_width_mismatch_is_an_error_not_a_panic() {
    let mut t = Topology::default();
    // The catalog builds "liar" as 1-in/1-out whatever the spec says.
    t.nodes.insert(A, spec("liar", 2, 3));
    t.outputs = vec![Source::Node(out(A, 0))];

    let err = compile_err(&t);
    let CompileError::WidthMismatch {
        node,
        declared,
        built,
        ..
    } = err
    else {
        panic!("expected a width mismatch, got {err:?}");
    };
    assert_eq!(node, A);
    assert_eq!(
        declared,
        (ChannelLayout::STEREO, ChannelLayout::from_count(3))
    );
    assert_eq!(built, (ChannelLayout::MONO, ChannelLayout::MONO));
}

/// A feedback edge validates — it legally breaks a cycle — and is refused by
/// `compile`, because `Net` has no feedback edge to lower it onto. The refusal
/// is explicit and names the port; see [`CompileError::FeedbackUnsupported`] for
/// why lowering it would mean synthesising a node the value cannot see.
///
/// Mutation: drop the feedback scan from `compile` → `set_source` writes a
/// cycle, `Net::error` goes to `Some(Cycle)`, and nothing here returns an error
/// → fails.
#[test]
fn a_feedback_edge_is_refused_explicitly() {
    let mut t = Topology::default();
    t.nodes.insert(A, spec("gain", 1, 1));
    t.nodes.insert(B, spec("gain", 1, 1));
    edge(&mut t, at(B, 0), out(A, 0));
    t.edges.insert(
        at(A, 0),
        Edge::Feedback(FeedbackFrom::one_block(out(B, 0), Samples(64))),
    );
    t.outputs = vec![Source::Node(out(B, 0))];

    t.validate().expect("feedback breaks the cycle");
    let err = compile_err(&t);
    assert_eq!(err, CompileError::FeedbackUnsupported { at: at(A, 0) });
}

// ---------------------------------------------------------------------------
// 4. The first Engine end-to-end test.
// ---------------------------------------------------------------------------

/// Topology → compile → `Engine::process`, asserted on the **samples**.
///
/// `tutti-core`'s `Engine` had no end-to-end test before this: an assertion on
/// what it renders needed a graph, and building a graph meant `bevy_tutti`'s
/// `App` plus a device. A `Topology` needs neither, which is the practical half
/// of what the value layer buys.
///
/// The graph is `dc(0.5) → gain(0.25) → both output channels`, so every rendered
/// sample must be exactly 0.125 — a number the value predicts and nothing in the
/// render path can round.
///
/// Mutation: swap the gain node's spec param to 0.5 without touching the
/// assertion → every sample is 0.25 → fails. Mutation: have `compile` skip
/// `set_output_source` → the buffer stays silent → fails.
#[test]
fn an_engine_renders_the_value_it_was_compiled_from() {
    let mut t = Topology::default();
    t.nodes.insert(
        A,
        spec("dc", 0, 1).with_param("value", ParamValue::Scalar(0.5)),
    );
    t.nodes.insert(
        B,
        spec("gain", 1, 1).with_param("gain", ParamValue::Scalar(0.25)),
    );
    edge(&mut t, at(B, 0), out(A, 0));
    t.outputs = vec![Source::Node(out(B, 0)), Source::Node(out(B, 0))];

    let valid = t.validate().expect("well formed");
    let mut net = compile(&valid, &TestCatalog, RATE)
        .expect("catalog builds it")
        .net;

    // The transport clock is pushed after compiling, the way any host adds an
    // engine-owned node: it is not authored, so it is not in the document and
    // must not be in the value. (Membership rule — see docs/design/003.)
    let transport = Transport::new(RATE.get());
    net.push(Box::new(TransportClock::new(
        transport.clock_links(),
        RATE.get(),
    )));

    let backend = net.backend();
    // The backend holds a pointer back into the net; keep the net alive for the
    // whole render, exactly as `rt_no_alloc_engine` does.
    let _keep: &'static Mutex<Net> = Box::leak(Box::new(Mutex::new(net)));

    let engine = Engine::new(transport.motion.clone(), backend);
    transport
        .motion
        .try_send(MotionEvent::Play)
        .expect("the queue is empty");

    let mut output = vec![0.0f32; 256 * 2];
    let mut rendered = 0usize;
    for _ in 0..8 {
        engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO));
        for s in &output {
            assert_eq!(*s, 0.125, "dc(0.5) through gain(0.25)");
        }
        rendered += output.len() / 2;
    }
    assert_eq!(rendered, 8 * 256);

    // And the transport advanced, so the render actually ran rather than the
    // buffer having been born at 0.125.
    let beat = transport.settings.beat().get();
    let expected = 8.0 * 256.0 / RATE.get() * (120.0 / 60.0);
    assert!(
        (beat - expected).abs() < 1e-3,
        "the playhead moved {beat} beats, expected about {expected}"
    );

    // The value predicted a silent-free render; so does its latency plan.
    assert!(latency::plan(valid.get()).is_empty());
}

/// The catalog is handed the sample rate, and a node built from it sees the same
/// rate the `Net` runs at.
///
/// Mutation: pass `SampleRate(44_100.0)` to `Catalog::build` regardless of the
/// argument → fails.
#[test]
fn the_catalog_is_given_the_rate_the_net_runs_at() {
    struct RateRecorder(Arc<Mutex<Vec<f64>>>);

    impl Catalog for RateRecorder {
        fn build(&self, spec: &NodeSpec, sample_rate: SampleRate) -> Option<Box<dyn AudioUnit>> {
            self.0.lock().push(sample_rate.get());
            TestCatalog.build(spec, sample_rate)
        }
    }

    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.outputs = vec![Source::Node(out(A, 0))];

    let valid = t.validate().expect("well formed");
    let built = compile(
        &valid,
        &RateRecorder(Arc::clone(&seen)),
        SampleRate(96_000.0),
    )
    .expect("builds");

    assert_eq!(&*seen.lock(), &[96_000.0]);
    assert_eq!(built.net.sample_rate(), 96_000.0);
}

/// The global input arity of the compiled `Net` is the value's own, so a
/// `Source::Global` edge resolves to the channel the value names.
///
/// Mutation: hard-code `Net::new(0, …)` → the `global_input` shape's
/// `set_source` asserts on the channel index → fails as a panic.
#[test]
fn the_nets_input_arity_is_the_values() {
    let shapes = shapes();
    let (_, t) = shapes
        .iter()
        .find(|(n, _)| *n == "global_input")
        .expect("shape present");
    let (_, built) = compiled(t);

    assert_eq!(AudioUnit::inputs(&built.net), 2);
    // And a topology with no inputs compiles to a net with none, which is what
    // a master graph is.
    let (_, chain) = shapes.iter().find(|(n, _)| *n == "chain").unwrap();
    let (_, built) = compiled(chain);
    assert_eq!(AudioUnit::inputs(&built.net), 0);
}
