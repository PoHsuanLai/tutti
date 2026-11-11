//! Edge gain, stage G2 (doc 013, "Edge gain: a mixer is a weighted sum"): an
//! edge's gain can be **driven** by an event output (a control lane,
//! `GraphSpec::connect_edge_gain`), with the param port's rules, and its
//! source is aligned to the edge's sink (`DelayKey::EdgeGain`).
//!
//! Every figure is analytic: the expected render is the fold with the weight
//! the driver's steps give at each frame, written out in the test. Executor
//! and reference are compared at every block (`Pair`), and the parallel
//! executor against the serial one, bit for bit.
//!
//! Mutations are recorded on each test (each run, each failed, then restored).

mod common;

use std::collections::BTreeMap;

use common::{Kind, Pair, SpecBehaviour, bits, driver_value, input_signal, prepare};
use tutti_graph::{
    CompileError, Edge, EdgeId, Editor, EventOut, ForkCause, ForkMode, ForkSource, Forked,
    GraphInvalid, GraphSpec, InPort, IntoNode, Node, NodeKey, NodeParts, NodeSpec, OutPort,
    PARAM_DECLICK, ParamRange, Sink, Source, Topology, Unforkable, ValidGraph, compile,
};
use tutti_types::{Amplitude, Samples, Tail};

const BLOCK: usize = 32;
const PERIOD: u64 = 16;

fn key(n: u64) -> NodeKey {
    NodeKey(n)
}

fn at(node: u64) -> InPort {
    InPort {
        node: key(node),
        port: 0,
    }
}

fn out(node: u64) -> Source {
    Source::Node(OutPort {
        node: key(node),
        port: 0,
    })
}

fn lane_out(node: u64) -> EventOut {
    EventOut {
        node: key(node),
        port: 0,
    }
}

fn amp(v: f32) -> Amplitude {
    Amplitude::new(v)
}

fn dc(value: f32) -> Kind {
    Kind::Const { value, width: 1 }
}

/// A source with a declared latency it does not realise (its audio is there
/// from frame 0; the compiler aligns what meets it).
fn latent_dc(value: f32, latency: usize) -> Kind {
    Kind::Spec {
        behaviour: SpecBehaviour::Dc(value),
        ins: 0,
        outs: 1,
        latency,
        tail: Tail::None,
    }
}

fn driver(latency: usize) -> Kind {
    Kind::GainDriver {
        period: PERIOD,
        latency,
    }
}

fn spec_of(kind: &Kind) -> NodeSpec {
    let s = Node::shape(&common::TestNode::new(kind.clone()));
    NodeSpec::new("test", s.audio_in(), s.audio_out())
        .with_latency(s.latency.samples())
        .with_tail(s.tail)
}

/// Where the summed edge is: at a pass-through's port (node `sink`), or at
/// global output channel 0.
#[derive(Clone, Copy, Debug)]
enum Where {
    Port,
    Output,
}

/// `kinds[0]` is node 1, `kinds[1]` node 2, ...: the audio sources are the
/// nodes in `sources`, summed at `at`, and the node `lane` is a driver.
struct Rig {
    kinds: BTreeMap<NodeKey, Kind>,
    spec: GraphSpec,
    at: Where,
    sink: u64,
}

impl Rig {
    fn new(kinds: &[Kind], sources: &[u64], at: Where) -> Self {
        let mut kinds: Vec<Kind> = kinds.to_vec();
        let sink = kinds.len() as u64 + 1;
        kinds.push(Kind::Thru { width: 1 });
        let mut t = Topology::default();
        let mut map = BTreeMap::new();
        for (i, k) in kinds.iter().enumerate() {
            t.nodes.insert(key(i as u64 + 1), spec_of(k));
            map.insert(key(i as u64 + 1), k.clone());
        }
        match at {
            Where::Port => {
                for &s in sources {
                    t.connect(at_port(sink), Edge::Direct(out(s)));
                }
                t.set_outputs(vec![out(sink)]);
            }
            Where::Output => {
                t.outputs = vec![sources.iter().map(|&s| out(s)).collect()];
            }
        }
        Self {
            kinds: map,
            spec: GraphSpec::new(t),
            at,
            sink,
        }
    }

    fn edge(&self, from: u64) -> EdgeId {
        EdgeId {
            sink: match self.at {
                Where::Port => Sink::Port(at_port(self.sink)),
                Where::Output => Sink::Output(0),
            },
            from: Edge::Direct(out(from)),
        }
    }

    /// Source `from`'s edge becomes a lane driven by `lane`.
    fn drive(&mut self, from: u64, lane: u64) -> EdgeId {
        let id = self.edge(from);
        self.spec.connect_edge_gain(id, lane_out(lane));
        id
    }

    fn graph(&self) -> ValidGraph {
        self.spec.validate().expect("valid")
    }
}

fn at_port(node: u64) -> InPort {
    at(node)
}

/// Render `blocks` blocks on `workers` participants, asserting each is the
/// reference's.
fn render(rig: &Rig, workers: usize, blocks: usize) -> Vec<f32> {
    let mut pair = Pair::with_workers(BLOCK, workers);
    pair.switch(&rig.graph(), &rig.kinds);
    let mut got = Vec::new();
    for b in 0..blocks {
        let input = input_signal((b * BLOCK) as u64, BLOCK);
        let (a, r) = pair.block(BLOCK, &input);
        assert_eq!(bits(&a), bits(&r), "executor and reference diverge");
        got.extend(&a[0]);
    }
    got
}

/// `blocks` more blocks of `pair`, each checked against the reference.
fn run(pair: &mut Pair, blocks: usize) -> Vec<f32> {
    let mut got = Vec::new();
    for b in 0..blocks {
        let input = input_signal((b * BLOCK) as u64, BLOCK);
        let (a, r) = pair.block(BLOCK, &input);
        assert_eq!(bits(&a), bits(&r), "executor and reference diverge");
        got.extend(&a[0]);
    }
    got
}

fn render_both(rig: &Rig, blocks: usize) -> Vec<f32> {
    let serial = render(rig, 1, blocks);
    let parallel = render(rig, 4, blocks);
    assert_eq!(
        serial.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
        parallel.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
        "parallel differs from serial"
    );
    serial
}

/// The weight a driver of this `latency` offset gives frame `n`: the step of
/// the last multiple of `PERIOD` it has sent by then (`shift` frames late).
fn stepped(n: usize, shift: usize) -> Option<f32> {
    let f = n.checked_sub(shift)? as u64;
    Some(driver_value(f - f % PERIOD, PERIOD))
}

fn delay_of(plan: &tutti_graph::Plan, key: tutti_graph::DelayKey) -> Option<usize> {
    plan.delays()
        .iter()
        .find(|d| d.key == key)
        .map(|d| d.len.get())
}

fn plan_of(rig: &Rig) -> tutti_graph::Plan {
    compile(
        &rig.graph(),
        &common::shapes_of(&rig.kinds),
        &prepare(BLOCK),
        None,
    )
    .expect("compiles")
    .0
}

// ---------------------------------------------------------------------------
// The driven lane's value.
// ---------------------------------------------------------------------------

/// A lane driving an edge's gain renders the weight the driver steps give at
/// every frame: `((+0.0 + w[n] * 1.0) + 0.5)`, at a port and at an output
/// channel, serial, parallel and reference. The other edge stays plain.
///
/// Mutations (each run, each failed): `SumLane::weight` returning
/// `Weight::Static(to)` where it has frames -> the weight never moves; read
/// the lane's events from slot 0 (`events.ev(0)`) in `sum_op` -> the lane
/// never hears its source (every driven test fails).
#[test]
fn a_driven_lane_renders_the_analytic_weight() {
    for at in [Where::Port, Where::Output] {
        let mut rig = Rig::new(&[dc(1.0), dc(0.5), driver(0)], &[1, 2], at);
        rig.drive(1, 3);
        let got = render_both(&rig, 6);
        for (n, &g) in got.iter().enumerate() {
            let w = stepped(n, 0).expect("the first step is on frame 0");
            let want = (0.0 + w * 1.0) + 0.5;
            assert_eq!(g.to_bits(), want.to_bits(), "{at:?} frame {n}");
        }
    }
}

/// The driven value is clamped to the entry's range, frame by frame (the
/// base is not: it reads before the first event).
///
/// Mutation (run, failed): `GainCell::store_range` and `GainCell::new` storing
/// `ParamRange::UNBOUNDED` -> the steps pass through unclamped.
#[test]
fn a_driven_lane_is_clamped_to_its_range() {
    let mut rig = Rig::new(&[dc(1.0), driver(0)], &[1], Where::Output);
    let id = rig.drive(1, 2);
    rig.spec.gains.get_mut(&id).expect("entry").range = ParamRange::new(0.0, 0.5);
    let got = render_both(&rig, 4);
    for (n, &g) in got.iter().enumerate() {
        let w = stepped(n, 0).expect("step").clamp(0.0, 0.5);
        assert_eq!(g.to_bits(), (0.0 + w * 1.0).to_bits(), "frame {n}");
    }
}

/// Before its first event a driven lane reads its base, and a source that
/// goes crossfades from where it was to the base over `PARAM_DECLICK` frames
/// (the param port's rule, one copy). One constant step, then a commit that
/// disconnects the source: frame `j` after it is
/// `base + (held - base) * (1 - (j + 1) / PARAM_DECLICK)`.
///
/// Mutations (each run, each failed): `(driven || state.busy())` -> `driven` in
/// `SumLane::weighted` -> the weight jumps to the base; carry no drive state
/// (`c.drive = None` after `SumLane::carry` in `rebuild`) -> likewise.
#[test]
fn a_source_that_goes_is_declicked_to_the_base() {
    let base = 0.75f32;
    let mut rig = Rig::new(&[dc(1.0), driver(0)], &[1], Where::Output);
    // One step only (frame 0): a period past the run.
    rig.kinds.insert(
        key(2),
        Kind::GainDriver {
            period: 1 << 20,
            latency: 0,
        },
    );
    let id = rig.drive(1, 2);
    rig.spec.gains.get_mut(&id).expect("entry").base = amp(base);
    let held = driver_value(0, 1 << 20);

    let mut pair = Pair::new(BLOCK);
    pair.switch(&rig.graph(), &rig.kinds);
    let got = run(&mut pair, 2);
    assert!(got.iter().all(|&x| x == held), "held at the step: {held}");
    rig.spec.disconnect_edge_gain(id, lane_out(2));
    pair.switch(&rig.graph(), &rig.kinds);
    let got = run(&mut pair, PARAM_DECLICK.get() / BLOCK + 2);
    let d = PARAM_DECLICK.get();
    for (j, &g) in got.iter().enumerate() {
        let w = if j < d {
            base + (held - base) * (1.0 - (j + 1) as f32 / d as f32)
        } else {
            base
        };
        assert_eq!(g.to_bits(), (0.0 + w * 1.0).to_bits(), "frame {j}");
    }
}

/// A driven lane's state survives a commit that keeps its edge: the held
/// value does not fall back to the base.
///
/// Mutation (run, failed): `c.drive = None` after `SumLane::carry` in
/// `rebuild` -> after the first switch the lane reads its base.
#[test]
fn a_driven_lane_survives_a_commit_that_keeps_its_edge() {
    let mut rig = Rig::new(&[dc(1.0), driver(0)], &[1], Where::Output);
    rig.kinds.insert(
        key(2),
        Kind::GainDriver {
            period: 1 << 20,
            latency: 0,
        },
    );
    rig.drive(1, 2);
    let held = driver_value(0, 1 << 20);
    let mut pair = Pair::new(BLOCK);
    pair.switch(&rig.graph(), &rig.kinds);
    for round in 0..3 {
        let (a, r) = pair.block(BLOCK, &input_signal(0, BLOCK));
        assert_eq!(bits(&a), bits(&r));
        assert!(a[0].iter().all(|&x| x == held), "round {round}");
        pair.switch(&rig.graph(), &rig.kinds);
    }
}

// ---------------------------------------------------------------------------
// PDC.
// ---------------------------------------------------------------------------

/// A lane that arrives **late** raises its sink's arrival, so the audio is
/// delayed to it and the lane itself needs no ring. Worked by hand: the
/// driver declares 32 frames of latency, the source none, so the pass-through
/// arrives at 32; the audio gets a 32-frame ring (silent for the first 32
/// frames), the lane none, and frame `n >= 32` is `w[n] * 1.0` with the step
/// of its own frame.
///
/// Mutation (run, failed): drop `.chain(gain_sources)` from `driving_nodes` -> the
/// sink does not wait for the lane: no audio ring, and the render starts at
/// frame 0.
#[test]
fn a_late_lane_delays_the_audio_to_the_edge() {
    let mut rig = Rig::new(&[dc(1.0), driver(32)], &[1], Where::Port);
    let id = rig.drive(1, 2);
    let plan = plan_of(&rig);
    assert_eq!(
        delay_of(
            &plan,
            tutti_graph::DelayKey::Audio {
                at: at_port(3),
                from: out(1)
            }
        ),
        Some(32)
    );
    assert_eq!(
        delay_of(
            &plan,
            tutti_graph::DelayKey::EdgeGain {
                id,
                from: lane_out(2)
            }
        ),
        None
    );
    let got = render_both(&rig, 4);
    for (n, &g) in got.iter().enumerate() {
        let want = if n < 32 {
            0.0
        } else {
            0.0 + stepped(n, 0).expect("step") * 1.0
        };
        assert_eq!(g.to_bits(), want.to_bits(), "frame {n}");
    }
}

/// A lane that arrives **early** is delayed to the sink's arrival: the
/// source declares 48 frames of latency it does not realise, so the lane gets
/// a 48-frame event ring (`DelayKey::EdgeGain`) and its event stamped `f`
/// lands on frame `f + 48`; until then the lane reads its base. At a port and
/// at an output channel (where the output time is the sink's arrival).
///
/// Mutation (run, failed): `let by = Samples::ZERO` for the port-sink lane in
/// `compile` (the lane's event is not delayed) -> the steps land 48 frames
/// early and the ring is missing.
#[test]
fn an_early_lane_is_delayed_to_the_edge() {
    for at in [Where::Port, Where::Output] {
        let mut rig = Rig::new(&[latent_dc(1.0, 48), driver(0)], &[1], at);
        let id = rig.drive(1, 2);
        let plan = plan_of(&rig);
        assert_eq!(
            delay_of(
                &plan,
                tutti_graph::DelayKey::EdgeGain {
                    id,
                    from: lane_out(2)
                }
            ),
            Some(48),
            "{at:?}"
        );
        let got = render_both(&rig, 4);
        for (n, &g) in got.iter().enumerate() {
            let w = stepped(n, 48).unwrap_or(1.0);
            assert_eq!(g.to_bits(), (0.0 + w * 1.0).to_bits(), "{at:?} frame {n}");
        }
    }
}

/// A lane that arrives late at a **global output** raises the output time:
/// the source's audio is delayed to it (an `Output` ring of 32), as the lane
/// for a port raises a node's arrival.
///
/// Mutation (run, failed): `delays` chaining `std::iter::empty()` in place of
/// `g.output_sources()` -> the output time is 0 and the audio is not delayed.
#[test]
fn a_late_lane_raises_the_output_time() {
    let mut rig = Rig::new(&[dc(1.0), driver(32)], &[1], Where::Output);
    rig.drive(1, 2);
    let plan = plan_of(&rig);
    assert_eq!(plan.total_latency().samples(), Samples(32));
    assert_eq!(
        delay_of(
            &plan,
            tutti_graph::DelayKey::Output {
                channel: 0,
                from: out(1)
            }
        ),
        Some(32)
    );
    let got = render_both(&rig, 4);
    for (n, &g) in got.iter().enumerate() {
        let want = if n < 32 {
            0.0
        } else {
            0.0 + stepped(n, 0).expect("step") * 1.0
        };
        assert_eq!(g.to_bits(), want.to_bits(), "frame {n}");
    }
}

// ---------------------------------------------------------------------------
// What `connect_edge_gain` refuses.
// ---------------------------------------------------------------------------

/// `connect_edge_gain` on an edge that is not there, from a node that is not
/// there, from an output port the node does not have, or from a node
/// downstream of the sink, is refused by `validate` or `compile`.
///
/// Mutations (each run, each failed): skip the `GainWithoutEdge` push in `validate`
/// -> case 1 validates; skip the `UnknownGainNode` push -> case 2
/// validates; skip the port check in `compile` -> case 3 compiles; leave the
/// `preds` push out of the gain edge in `compile` -> case 4 compiles.
#[test]
fn connect_edge_gain_refusals() {
    // 1. No such edge.
    let mut rig = Rig::new(&[dc(1.0), driver(0)], &[1], Where::Port);
    let ghost = EdgeId {
        sink: Sink::Port(at_port(3)),
        from: Edge::Direct(out(2)),
    };
    rig.spec.connect_edge_gain(ghost, lane_out(2));
    assert!(matches!(
        rig.spec.validate().unwrap_err()[..],
        [GraphInvalid::GainWithoutEdge { .. }]
    ));

    // 2. A source node that is not in the graph.
    let mut rig = Rig::new(&[dc(1.0)], &[1], Where::Port);
    let id = rig.edge(1);
    rig.spec.connect_edge_gain(id, lane_out(99));
    assert!(matches!(
        rig.spec.validate().unwrap_err()[..],
        [GraphInvalid::UnknownGainNode { .. }]
    ));

    // 3. A port the node does not have (wrong port kind: it has no event
    // output 5).
    let mut rig = Rig::new(&[dc(1.0), driver(0)], &[1], Where::Port);
    let id = rig.edge(1);
    rig.spec.connect_edge_gain(
        id,
        EventOut {
            node: key(2),
            port: 5,
        },
    );
    let err = compile(
        &rig.graph(),
        &common::shapes_of(&rig.kinds),
        &prepare(BLOCK),
        None,
    )
    .unwrap_err();
    assert!(matches!(err, CompileError::GainSourceOutOfRange { .. }));

    // A node with no event output at all is the same refusal.
    let mut rig = Rig::new(&[dc(1.0), dc(2.0)], &[1], Where::Port);
    let id = rig.edge(1);
    rig.spec.connect_edge_gain(id, lane_out(2));
    let err = compile(
        &rig.graph(),
        &common::shapes_of(&rig.kinds),
        &prepare(BLOCK),
        None,
    )
    .unwrap_err();
    assert!(matches!(err, CompileError::GainSourceOutOfRange { .. }));

    // 4. A lane downstream of its own sink is a cycle: node 2 reads the
    // sink's output and drives the gain of an edge into the sink, so the sum
    // would wait for what waits for it.
    let lane = Kind::Mixed {
        width: 1,
        events_in: 0,
        events_out: 1,
    };
    let mut rig = Rig::new(&[dc(1.0), lane], &[1], Where::Port);
    let id = rig.edge(1);
    rig.spec.topology.connect(at(2), Edge::Direct(out(3)));
    rig.spec.connect_edge_gain(id, lane_out(2));
    let err = compile(
        &rig.graph(),
        &common::shapes_of(&rig.kinds),
        &prepare(BLOCK),
        None,
    )
    .unwrap_err();
    assert!(matches!(err, CompileError::Cycle { .. }), "{err:?}");
}

/// `connect_edge_gain` and `disconnect_edge_gain` work as the param port's
/// do: the first returns the source it replaced, the second says whether the
/// source was the one given and keeps the entry; a source is structural
/// (a commit recompiles), a range is a control (`set_edge_gain_range`: no
/// plan, and the cell a control lane reads moves with it).
///
/// Mutations (each run, each failed): `same_but_ranges` comparing no sources ->
/// connecting a source sends no plan; mark a commit in
/// `set_edge_gain_range` (`self.committed = None`) -> the plan `Arc` differs.
#[test]
fn a_source_is_structural_and_a_range_is_a_control() {
    let (mut ed, mut exec) = Editor::new(prepare(BLOCK));
    ed.insert(key(1), "dc", Unforkable(common::TestNode::new(dc(1.0))));
    ed.insert(key(2), "driver", Forkable(driver(0)));
    ed.spec_mut().topology.outputs = vec![vec![out(1)]];
    let id = EdgeId {
        sink: Sink::Output(0),
        from: Edge::Direct(out(1)),
    };
    ed.set_edge_gain(id, amp(1.0)).expect("the edge exists");
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    let plain = ed.base().expect("a plan").clone();
    let reader = ed.edge_gain_range_cell(id).expect("a lane's cell");

    assert_eq!(ed.spec_mut().connect_edge_gain(id, lane_out(2)), None);
    assert_eq!(
        ed.spec_mut().connect_edge_gain(id, lane_out(2)),
        Some(lane_out(2)),
        "replaces, and says what it replaced"
    );
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    let driven = ed.base().expect("a plan").clone();
    assert!(
        !std::sync::Arc::ptr_eq(&plain, &driven),
        "a source recompiles"
    );
    assert_eq!(driven.gain_ports().len(), 1);
    assert!(reader.same_cell(&ed.edge_gain_range_cell(id).expect("cell")));

    ed.set_edge_gain_range(id, ParamRange::new(-1.0, 1.0))
        .expect("an edge with an entry");
    assert_eq!(reader.load(), ParamRange::new(-1.0, 1.0));
    ed.commit().expect("commits");
    assert!(
        std::sync::Arc::ptr_eq(&driven, ed.base().expect("a plan")),
        "a range sends no plan"
    );
    assert!(matches!(
        ed.set_edge_gain_range(id, ParamRange::new(f32::NAN, 1.0)),
        Err(GraphInvalid::BadEdgeGain { .. })
    ));

    assert!(
        !ed.spec_mut().disconnect_edge_gain(id, lane_out(1)),
        "not its source"
    );
    assert!(ed.spec_mut().disconnect_edge_gain(id, lane_out(2)));
    assert!(ed.spec().gains.contains_key(&id), "the entry stays");
    // Removing the lane's node undrives the lane and keeps the edge.
    ed.spec_mut().connect_edge_gain(id, lane_out(2));
    ed.remove(key(2));
    assert_eq!(ed.spec().gains[&id].source, None);
    ed.commit().expect("commits");
}

/// A fork of the master carries a driven lane and its driver (the lane's
/// node is a root of the fork), and its first block starts at the driven
/// value.
///
/// Mutation (run, failed): `false &&` in front of the output-sink filter of
/// the Master roots in `fork_gather` -> the fork does not build
/// (`UnknownGainNode`).
#[test]
fn a_fork_carries_the_driven_lane() {
    use tutti_graph::ForkTarget;
    let (mut ed, mut exec) = Editor::new(prepare(BLOCK));
    ed.insert(key(1), "dc", Forkable(dc(1.0)));
    ed.insert(key(2), "driver", Forkable(driver(0)));
    ed.spec_mut().topology.outputs = vec![vec![out(1)]];
    let id = EdgeId {
        sink: Sink::Output(0),
        from: Edge::Direct(out(1)),
    };
    ed.set_edge_gain(id, amp(1.0)).expect("edge");
    ed.spec_mut().connect_edge_gain(id, lane_out(2));
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    let (mut fed, mut fexec) = ed
        .fork(ForkTarget::Master, ForkMode::Offline, prepare(BLOCK))
        .expect("forks");
    let mut o = vec![0.0f32; BLOCK];
    fexec.process(
        BLOCK,
        &tutti_graph::Transport::default(),
        &[],
        &mut [o.as_mut_slice()],
    );
    fed.collect();
    for (n, &x) in o.iter().enumerate() {
        let w = stepped(n, 0).expect("step");
        assert_eq!(x.to_bits(), (0.0 + w * 1.0).to_bits(), "frame {n}");
    }
}

/// A test node made forkable: a fork is a fresh node of the same kind.
struct Forkable(Kind);

struct KindFork(Kind);

impl ForkSource for KindFork {
    fn fork(&self, _mode: ForkMode) -> Result<Forked, ForkCause> {
        Ok(Forked::new(Box::new(common::TestNode::new(self.0.clone()))))
    }
}

impl IntoNode for Forkable {
    type Controls = ();
    fn into_node(self) -> (Box<dyn Node>, ()) {
        (Box::new(common::TestNode::new(self.0)), ())
    }
    fn into_parts(self) -> NodeParts<()> {
        NodeParts {
            node: Box::new(common::TestNode::new(self.0.clone())),
            controls: (),
            fork: Some(Box::new(KindFork(self.0))),
        }
    }
}
