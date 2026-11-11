//! A node's mix is a modulation target (doc 013, "Bypass and mute are graph
//! state", stage M4b): an event output (a control lane) drives it through
//! `GraphSpec::connect_param` at `NodeMix::ADDR`, with the param port's rules,
//! its source is aligned to the node (`DelayKey::ParamEvent`), and the mix cell
//! is the lane's one knob (`Editor::mix_cell`).
//!
//! Every figure is analytic: the expected render is `y * m + x̂ * (1 - m)` with
//! the `m` the driver's steps give at each frame, written out in the test.
//! Executor and reference are compared at every block (`Pair`), and the
//! parallel executor against the serial one, bit for bit.
//!
//! Mutations are recorded on each test (each run, each failed, then restored).

#![allow(
    clippy::needless_range_loop,
    reason = "a frame index reads several outputs and the signal at that frame"
)]

mod common;

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use common::{Kind, Pair, SpecBehaviour, TestNode, bits, driver_value, input_signal, prepare};
use tutti_graph::{
    CompileError, CycleEdge, Edge, Editor, EventOut, ForkMode, ForkTarget, GraphInvalid, GraphSpec,
    InPort, Node, NodeKey, NodeMix, NodeMode, NodeSpec, OutPort, PARAM_DECLICK, ParamIn,
    ParamPorts, ParamPortsError, ParamRange, Source, Topology, Transport, ValidGraph, compile,
};
use tutti_types::{ChannelLayout, Mix, Tail, UnitAddr, UnitParam};

const BLOCK: usize = 32;
const PERIOD: u64 = 16;
/// The declared latency of the latent node under test.
const L: usize = 48;

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

fn mix_of(node: u64) -> ParamIn {
    ParamIn {
        node: key(node),
        param: NodeMix::ADDR,
    }
}

fn spec_of(kind: &Kind) -> NodeSpec {
    let s = TestNode::new(kind.clone()).shape();
    NodeSpec::new("test", s.audio_in, s.audio_out)
        .with_latency(s.latency.samples())
        .with_tail(s.tail)
}

/// `3 * x`, no latency: the node under test.
fn plain() -> Kind {
    Kind::Gain {
        gain: 3.0,
        width: 1,
    }
}

/// `3 * x`, its output not delayed, with a declared latency of `L`: its
/// aligned input is `x(n - L)`.
fn latent() -> Kind {
    Kind::Spec {
        behaviour: SpecBehaviour::Gain(3.0),
        ins: 1,
        outs: 1,
        latency: L,
        tail: Tail::None,
    }
}

fn driver(latency: usize) -> Kind {
    Kind::MixDriver {
        period: PERIOD,
        latency,
    }
}

/// A source with a declared latency it does not realise: its audio is there
/// from frame 0, and the compiler aligns what meets it.
fn latent_dc(value: f32, latency: usize) -> Kind {
    Kind::Spec {
        behaviour: SpecBehaviour::Dc(value),
        ins: 0,
        outs: 1,
        latency,
        tail: Tail::None,
    }
}

/// `y * m + x̂ * (1 - m)`: the exact expression the executor writes.
fn blend(y: f32, x_hat: f32, m: f32) -> f32 {
    y * m + x_hat * (1.0 - m)
}

/// What the driver's steps say at frame `n`, `shift` frames late: its last
/// step (doubled: `-1.0` to `2.0`, off the blend on both sides).
fn stepped(n: usize, shift: usize) -> Option<f32> {
    let f = n.checked_sub(shift)? as u64;
    Some(driver_value(f - f % PERIOD, PERIOD) * 2.0)
}

/// Node 1 is the driver, node 2 the node under test (fed by global input 0,
/// or by node 3 when there is one), and the mix of node 2 is driven by node 1.
struct Rig {
    kinds: BTreeMap<NodeKey, Kind>,
    spec: GraphSpec,
}

impl Rig {
    fn new(node: Kind, driver_latency: usize, source: Option<Kind>) -> Self {
        let mut kinds = BTreeMap::from([(key(1), driver(driver_latency)), (key(2), node)]);
        let mut t = Topology {
            inputs: ChannelLayout::MONO,
            ..Topology::default()
        };
        match source {
            Some(kind) => {
                kinds.insert(key(3), kind);
                t.nodes.insert(key(3), spec_of(&kinds[&key(3)]));
                t.connect(at(2), Edge::Direct(out(3)));
            }
            None => t.connect(at(2), Edge::Direct(Source::Global(0))),
        }
        for (&k, kind) in &kinds {
            t.nodes.insert(k, spec_of(kind));
        }
        t.set_outputs(vec![out(2)]);
        let mut spec = GraphSpec::new(t);
        spec.connect_param(mix_of(2), lane_out(1));
        Self { kinds, spec }
    }

    fn graph(&self) -> ValidGraph {
        self.spec.validate().expect("valid")
    }
}

/// Render `blocks` blocks of `BLOCK` frames on `workers` participants,
/// asserting each is the reference's; the node's output and the input.
fn render(rig: &Rig, workers: usize, blocks: usize) -> (Vec<f32>, Vec<f32>) {
    let mut pair = Pair::with_workers(BLOCK, workers);
    pair.switch(&rig.graph(), &rig.kinds);
    run(&mut pair, 0, blocks)
}

/// `blocks` more blocks of `pair` from frame `from`.
fn run(pair: &mut Pair, from: usize, blocks: usize) -> (Vec<f32>, Vec<f32>) {
    let (mut got, mut x) = (Vec::new(), Vec::new());
    for b in 0..blocks {
        let input = input_signal((from + b * BLOCK) as u64, BLOCK);
        let (a, r) = pair.block(BLOCK, &input);
        assert_eq!(bits(&a), bits(&r), "executor and reference diverge");
        got.extend(&a[0]);
        x.extend(&input);
    }
    (got, x)
}

/// Serial and four participants, bit for bit; the serial render.
fn render_both(rig: &Rig, blocks: usize) -> (Vec<f32>, Vec<f32>) {
    let serial = render(rig, 1, blocks);
    let parallel = render(rig, 4, blocks);
    assert_eq!(
        serial.0.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
        parallel.0.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
        "parallel differs from serial"
    );
    serial
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
// The driven value.
// ---------------------------------------------------------------------------

/// A lane driving a node's mix renders the blend at the mix the driver's steps
/// give at every frame (clamped to the blend), for a node with no latency and
/// for a latent one (whose aligned input is its input `L` frames late), serial,
/// parallel and reference.
///
/// Mutations (each run, each failed): `drive_step` never called in
/// `moded_node_op` -> the mix reads its base (WET) and the render is `3 * x`;
/// `ModeState::lane` returning `&[]` -> `MixRun::at` indexes an empty lane
/// (panic); `MixRun::driven` always false in `mix_run` -> the block ramps the
/// base and the render is the static blend; `moded_node_op` reading the lane's
/// events from slot 0 -> the lane never hears its driver.
#[test]
fn a_driven_mix_renders_the_analytic_blend() {
    for (kind, delay) in [(plain(), 0), (latent(), L)] {
        let rig = Rig::new(kind, 0, None);
        let (got, x) = render_both(&rig, 6);
        for (n, &g) in got.iter().enumerate() {
            let m = stepped(n, 0)
                .expect("the first step is on frame 0")
                .clamp(0.0, 1.0);
            let x_hat = n.checked_sub(delay).map_or(0.0, |i| x[i]);
            let want = blend(3.0 * x[n], x_hat, m);
            assert_eq!(g.to_bits(), want.to_bits(), "delay {delay}, frame {n}");
        }
    }
}

/// A driven block is never taken for a static one because its ends agree: with
/// a block of seven driver periods (112 frames) the lane's last frame is the
/// same step every block, so the previous block's value and this block's are
/// equal while the frames between differ (the differential suite found it).
///
/// Mutation (run, failed): `MixRun::is_static` without its `!self.driven` term
/// -> from the second block the whole block blends at the one value of its last
/// frame.
#[test]
fn a_driven_block_whose_ends_agree_is_not_static() {
    const SEVEN: usize = 7 * PERIOD as usize;
    let rig = Rig::new(plain(), 0, None);
    let mut pair = Pair::new(SEVEN);
    pair.switch(&rig.graph(), &rig.kinds);
    for b in 0..4 {
        let input = input_signal((b * SEVEN) as u64, SEVEN);
        let (a, r) = pair.block(SEVEN, &input);
        assert_eq!(bits(&a), bits(&r), "executor and reference diverge");
        for n in 0..SEVEN {
            let f = b * SEVEN + n;
            let m = stepped(f, 0).expect("step").clamp(0.0, 1.0);
            assert_eq!(
                a[0][n].to_bits(),
                blend(3.0 * input[n], input[n], m).to_bits(),
                "block {b}, frame {n}"
            );
        }
    }
}

/// The lane's value is clamped to `0..=1` whatever range the entry holds: the
/// default (`ParamMod::default` is unbounded, narrowed to the blend), a range
/// inside it, and one wider than it (`NodeMix::lane_range`).
///
/// Mutations (each run, each failed): `lane_range` returning its argument ->
/// the default and the wide range let `-1.0` and `2.0` through (off the
/// blend); the range cell not the lane's (`ParamState::lane` given a fresh
/// `ParamRanges` in `set_driven`) -> the narrow range is ignored.
#[test]
fn a_driven_mix_is_clamped_to_its_range_and_to_the_blend() {
    let wide = ParamRange::new(-5.0, 5.0);
    let narrow = ParamRange::new(0.25, 0.75);
    for (range, lo, hi) in [
        (None, 0.0, 1.0),
        (Some(wide), 0.0, 1.0),
        (Some(narrow), 0.25, 0.75),
    ] {
        let mut rig = Rig::new(plain(), 0, None);
        if let Some(r) = range {
            rig.spec.set_param_range(mix_of(2), r);
        }
        let (got, x) = render_both(&rig, 4);
        for (n, &g) in got.iter().enumerate() {
            let m = stepped(n, 0).expect("step").clamp(lo, hi);
            assert_eq!(
                g.to_bits(),
                blend(3.0 * x[n], x[n], m).to_bits(),
                "range {range:?}, frame {n}"
            );
        }
    }
}

/// A range moved between blocks (`set_param_range` at the mix's address)
/// clamps from the next block on, the cell the editor writes being the one the
/// lane reads.
///
/// Mutation (run, failed): `Editor::set_param_range` storing a mix range into
/// the node's declared cells rather than the mode cell -> the move never lands
/// and the second half is unclamped.
#[test]
fn a_range_moved_between_blocks_clamps_from_the_next() {
    let rig = Rig::new(plain(), 0, None);
    let mut pair = Pair::new(BLOCK);
    pair.switch(&rig.graph(), &rig.kinds);
    let (first, _) = run(&mut pair, 0, 2);
    pair.set_mix_range(key(2), ParamRange::new(0.5, 0.5));
    let (second, x) = run(&mut pair, 2 * BLOCK, 2);
    let (_, x0) = (0, input_signal(0, 2 * BLOCK));
    for (n, &g) in first.iter().enumerate() {
        let m = stepped(n, 0).expect("step").clamp(0.0, 1.0);
        assert_eq!(
            g.to_bits(),
            blend(3.0 * x0[n], x0[n], m).to_bits(),
            "frame {n}"
        );
    }
    for (n, &g) in second.iter().enumerate() {
        assert_eq!(
            g.to_bits(),
            blend(3.0 * x[n], x[n], 0.5).to_bits(),
            "frame {n}"
        );
    }
}

/// Before its first event a driven mix reads its base, and a source that goes
/// declicks from where it was to the base over `PARAM_DECLICK` frames (the
/// param port's rule, one copy): one step (frame 0's, `-1.0`, clamped to
/// `0.0`) held, then a commit that
/// disconnects the source. Frame `j` after it is
/// `base + (held - base) * (1 - (j + 1) / PARAM_DECLICK)`, and after that the
/// base: the node is the static blend (and, once the base is WET, plain again).
///
/// Mutations (each run, each failed): `set_driven` dropping the state of a
/// lane that is no longer driven (`self.drive = None`) -> the mix jumps to the
/// base; `snapshot` not resetting `lane_on` -> the block after the fade reads
/// a stale lane (a panic or a frozen mix).
#[test]
fn a_source_that_goes_is_declicked_to_the_base() {
    let base = 0.75f32;
    let mut rig = Rig::new(plain(), 0, None);
    // One step only (frame 0): a period past the run.
    rig.kinds.insert(
        key(1),
        Kind::MixDriver {
            period: 1 << 20,
            latency: 0,
        },
    );
    rig.spec.set_mix(key(2), Mix::new(base));
    let held = (driver_value(0, 1 << 20) * 2.0).clamp(0.0, 1.0);

    let mut pair = Pair::new(BLOCK);
    pair.switch(&rig.graph(), &rig.kinds);
    let (got, x) = run(&mut pair, 0, 2);
    for (n, &g) in got.iter().enumerate() {
        assert_eq!(
            g.to_bits(),
            blend(3.0 * x[n], x[n], held).to_bits(),
            "held, frame {n}"
        );
    }
    rig.spec.disconnect_param(mix_of(2), lane_out(1));
    pair.switch(&rig.graph(), &rig.kinds);
    let d = PARAM_DECLICK.get();
    let (got, x) = run(&mut pair, 2 * BLOCK, d / BLOCK + 2);
    for (j, &g) in got.iter().enumerate() {
        let m = if j < d {
            base + (held - base) * (1.0 - (j + 1) as f32 / d as f32)
        } else {
            base
        };
        assert_eq!(
            g.to_bits(),
            blend(3.0 * x[j], x[j], m).to_bits(),
            "frame {j}"
        );
    }
}

/// A driven lane's state survives a commit that keeps the node: the held value
/// does not fall back to the base.
///
/// Mutation (run, failed): `set_driven` rebuilding the lane's state on every
/// apply (`self.drive = None` first) -> after the first switch the mix reads
/// its base.
#[test]
fn a_driven_lane_survives_a_commit_that_keeps_its_node() {
    let mut rig = Rig::new(plain(), 0, None);
    rig.kinds.insert(
        key(1),
        Kind::MixDriver {
            period: 1 << 20,
            latency: 0,
        },
    );
    rig.spec.set_mix(key(2), Mix::new(0.25));
    let held = (driver_value(0, 1 << 20) * 2.0).clamp(0.0, 1.0);
    let mut pair = Pair::new(BLOCK);
    pair.switch(&rig.graph(), &rig.kinds);
    for round in 0..3 {
        let input = input_signal(0, BLOCK);
        let (a, r) = pair.block(BLOCK, &input);
        assert_eq!(bits(&a), bits(&r));
        for n in 0..BLOCK {
            let want = blend(3.0 * input[n], input[n], held);
            assert_eq!(
                a[0][n].to_bits(),
                want.to_bits(),
                "round {round}, frame {n}"
            );
        }
        pair.switch(&rig.graph(), &rig.kinds);
    }
}

/// **Mode wins over a driven mix**: a bypassed node writes its aligned input
/// whatever the lane says, and un-bypassing fades to the lane's value at each
/// frame, the lane having run all the while.
///
/// Mutation (run, failed): `drive_step` moved after the parked early return
/// in `moded_node_op` (a parked node's lane is not stepped) -> the lane's state is
/// stale when the node comes back and the reference (which steps it every
/// block) diverges.
#[test]
fn the_mode_wins_over_a_driven_mix() {
    let rig = Rig::new(plain(), 0, None);
    let mut pair = Pair::new(BLOCK);
    pair.switch(&rig.graph(), &rig.kinds);
    pair.set_mode(key(2), NodeMode::Bypassed);
    let (got, x) = run(&mut pair, 0, 14);
    // After the declick the bypassed node is the input, whatever the lane.
    for n in 256..got.len() {
        assert_eq!(got[n].to_bits(), x[n].to_bits(), "bypassed, frame {n}");
    }
    pair.set_mode(key(2), NodeMode::Active);
    let from = 14 * BLOCK;
    let (got, x) = run(&mut pair, from, 12);
    // Past the fade (and the warm-up of a node with no latency: none), the
    // blend at the lane's mix.
    for n in 256..got.len() {
        let m = stepped(from + n, 0).expect("step").clamp(0.0, 1.0);
        assert_eq!(
            got[n].to_bits(),
            blend(3.0 * x[n], x[n], m).to_bits(),
            "active again, frame {n}"
        );
    }
}

// ---------------------------------------------------------------------------
// PDC.
// ---------------------------------------------------------------------------

/// A lane that arrives **late** raises its node's arrival, so the audio is
/// delayed to it and the lane itself needs no ring. Worked by hand: the driver
/// declares 32 frames of latency, the global input none, so node 2 arrives at
/// 32; its input gets a 32-frame ring (silent for the first 32 frames), the
/// lane none, and frame `n >= 32` is the blend of that delayed input at the
/// step of its own frame.
///
/// Mutation (run, failed): leave the mix's `ParamIn` out of `driving_nodes` ->
/// the node does not wait for the lane: no audio ring and the render starts at
/// frame 0.
#[test]
fn a_late_lane_delays_the_audio_to_the_node() {
    let rig = Rig::new(plain(), 32, None);
    let plan = plan_of(&rig);
    assert_eq!(
        delay_of(
            &plan,
            tutti_graph::DelayKey::Audio {
                at: at(2),
                from: Source::Global(0)
            }
        ),
        Some(32)
    );
    assert_eq!(
        delay_of(
            &plan,
            tutti_graph::DelayKey::ParamEvent {
                at: mix_of(2),
                from: lane_out(1)
            }
        ),
        None
    );
    let (got, x) = render_both(&rig, 4);
    for (n, &g) in got.iter().enumerate() {
        let want = if n < 32 {
            0.0
        } else {
            let m = stepped(n, 0).expect("step").clamp(0.0, 1.0);
            blend(3.0 * x[n - 32], x[n - 32], m)
        };
        assert_eq!(g.to_bits(), want.to_bits(), "frame {n}");
    }
}

/// A lane that arrives **early** is delayed to the node's arrival: the source
/// declares 48 frames of latency it does not realise, so the lane gets a
/// 48-frame event ring (`DelayKey::ParamEvent` at the mix's address) and its
/// event stamped `f` lands on frame `f + 48`; until then the lane reads the
/// base (the set mix).
///
/// Mutation (run, failed): align the mix's source with `by = 0` in `compile` ->
/// the steps land 48 frames early and the ring is missing.
#[test]
fn an_early_lane_is_delayed_to_the_node() {
    let mut rig = Rig::new(plain(), 0, Some(latent_dc(1.0, 48)));
    rig.spec.set_mix(key(2), Mix::new(0.25));
    let plan = plan_of(&rig);
    assert_eq!(
        delay_of(
            &plan,
            tutti_graph::DelayKey::ParamEvent {
                at: mix_of(2),
                from: lane_out(1)
            }
        ),
        Some(48)
    );
    let (got, _) = render_both(&rig, 4);
    for (n, &g) in got.iter().enumerate() {
        let m = stepped(n, 48).map_or(0.25, |m| m.clamp(0.0, 1.0));
        assert_eq!(g.to_bits(), blend(3.0, 1.0, m).to_bits(), "frame {n}");
    }
}

// ---------------------------------------------------------------------------
// The refusals.
// ---------------------------------------------------------------------------

/// `connect_param` at the mix's address is refused as any param is, and the
/// address is the graph's alone: a lane at a node that is not there is
/// `UnknownParamNode`, a NaN range `BadParamRange`, a source port the node does
/// not have `ParamSourceOutOfRange`, a lane from a node the mix feeds a cycle
/// (a param edge in it), a node that declares the reserved address cannot
/// (`ParamPortsError::Reserved`), and an address no node declares other than
/// the mix's is still `UnknownParam`.
///
/// Mutations (each run, each failed): `UnknownParam`'s exemption widened to any
/// index of `Wet` -> the last assertion compiles; the `Reserved` check removed
/// from `try_of` -> the declaration is accepted; the mix left out of the
/// cycle's `preds` (it is the loop over `graph.params()`, which this edge is
/// in: replace it with a filter on `at.param != NodeMix::ADDR`) -> the cycle
/// compiles.
#[test]
fn the_mix_port_is_refused_as_a_param_is() {
    let rig = Rig::new(plain(), 0, None);
    let shapes = common::shapes_of(&rig.kinds);

    let mut ghost = rig.spec.clone();
    ghost.connect_param(mix_of(9), lane_out(1));
    assert!(matches!(
        ghost.validate().unwrap_err()[..],
        [GraphInvalid::UnknownParamNode { missing, .. }] if missing == key(9)
    ));

    let mut nan = rig.spec.clone();
    nan.set_param_range(mix_of(2), ParamRange::new(f32::NAN, 1.0));
    assert!(matches!(
        nan.validate().unwrap_err()[..],
        [GraphInvalid::BadParamRange { at, .. }] if at == mix_of(2)
    ));

    let mut port = rig.spec.clone();
    port.connect_param(
        mix_of(2),
        EventOut {
            node: key(1),
            port: 3,
        },
    );
    let err = compile(
        &port.validate().expect("valid"),
        &shapes,
        &prepare(BLOCK),
        None,
    )
    .expect_err("the driver has one event output");
    assert!(matches!(err, CompileError::ParamSourceOutOfRange { at, .. } if at == mix_of(2)));

    // Node 2 emits events too (`Emitter`); a lane from it into node 1's mix,
    // with node 1's lane into node 2's, is a loop.
    let mut loop_rig = Rig::new(plain(), 0, None);
    loop_rig.kinds.insert(
        key(2),
        Kind::Emitter {
            period: 4,
            phase: 0,
        },
    );
    loop_rig
        .spec
        .topology
        .nodes
        .insert(key(2), spec_of(&loop_rig.kinds[&key(2)]));
    loop_rig.spec.topology.edges.clear();
    loop_rig.spec.topology.set_outputs(vec![out(2)]);
    loop_rig.spec.connect_param(mix_of(1), lane_out(2));
    let err = compile(
        &loop_rig.graph(),
        &common::shapes_of(&loop_rig.kinds),
        &prepare(BLOCK),
        None,
    )
    .expect_err("a mix lane in a loop");
    let CompileError::Cycle { edges } = err else {
        panic!("not a cycle: {err:?}");
    };
    assert!(
        edges
            .iter()
            .any(|e| matches!(e, CycleEdge::Param { at, .. } if at.param == NodeMix::ADDR))
    );

    // Another address nothing declares is as it was.
    let mut other = rig.spec.clone();
    other.connect_param(
        ParamIn {
            node: key(2),
            param: UnitAddr::new(UnitParam::Wet, 254),
        },
        lane_out(1),
    );
    let err = compile(
        &other.validate().expect("valid"),
        &shapes,
        &prepare(BLOCK),
        None,
    )
    .expect_err("no node declares it");
    assert!(matches!(err, CompileError::UnknownParam { .. }));

    // And a node cannot declare the mix's address.
    assert_eq!(
        ParamPorts::try_of(&[UnitAddr::from(UnitParam::Cutoff), NodeMix::ADDR]),
        Err(ParamPortsError::Reserved(NodeMix::ADDR))
    );
    assert!(ParamPorts::try_of(&[UnitAddr::new(UnitParam::Wet, 254)]).is_ok());
}

// ---------------------------------------------------------------------------
// One writer.
// ---------------------------------------------------------------------------

/// `3 * x`: the node of the editor tests.
#[derive(Clone)]
struct Triple;

impl tutti_graph::Node for Triple {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &tutti_graph::Prepare) {}
    fn process(
        &mut self,
        _: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        let n = io.frames();
        let (ins, mut outs) = io.split();
        for i in 0..n {
            outs.get(0)[i] = 3.0 * ins.get(0)[i];
        }
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
}

fn solo() -> (Editor, tutti_graph::Executor) {
    let (mut ed, mut exec) = Editor::new(prepare(BLOCK));
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    ed.insert(key(1), "triple", tutti_graph::ForkByClone(Triple));
    ed.spec_mut()
        .topology
        .connect(at(1), Edge::Direct(Source::Global(0)));
    ed.spec_mut().topology.set_outputs(vec![out(1)]);
    ed.set_mix(key(1), Mix::new(0.25)).expect("a node");
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    (ed, exec)
}

fn block(exec: &mut tutti_graph::Executor, input: &[f32]) -> Vec<f32> {
    let mut o = vec![0.0f32; input.len()];
    exec.process(
        input.len(),
        &Transport::default(),
        &[input],
        &mut [&mut o[..]],
    );
    o
}

/// **The mix cell is the one writer's**: `mix_cell` is the very cell `set_mix`
/// stores into, so a write through it (a lane's knob) moves the mix, `mix`
/// reads it, a commit that nothing moved leaves it (the spec's copy is
/// behind), a spec edit that moved lands over it, and a fork starts at it. The
/// range cell is the mode cell's too.
///
/// Mutations (each run, each failed): `ModeCell::knob` returning a fresh
/// `Arc` -> the write reaches nothing; `carry_mix` storing unconditionally ->
/// the commit puts the spec's `0.25` back; `Editor::mix` reading the spec ->
/// `mix` says `0.25`; the fork reading `spec.mixes` rather than `self.mix`
/// -> it starts at `0.25`; `param_range_cell` answering the declared cells ->
/// `None` for the mix.
#[test]
fn the_mix_cell_is_the_one_writer() {
    let (mut ed, mut exec) = solo();
    let input: Vec<f32> = input_signal(0, BLOCK);
    let knob = ed.mix_cell(key(1)).expect("a placed unit has a cell");
    block(&mut exec, &input);
    knob.store(0.75, Ordering::Release);
    assert_eq!(ed.mix(key(1)), Mix::new(0.75), "the cell is the mix");
    // Ramp block, then steady.
    block(&mut exec, &input);
    let o = block(&mut exec, &input);
    for n in 0..BLOCK {
        assert_eq!(
            o[n].to_bits(),
            blend(3.0 * input[n], input[n], 0.75).to_bits()
        );
    }
    // A commit that recompiles with nothing moved keeps the cell, and so does
    // one with nothing to send.
    ed.spec_mut()
        .topology
        .nodes
        .get_mut(&key(1))
        .expect("node")
        .kind = "triple again".into();
    for _ in 0..2 {
        ed.commit().expect("commits");
        exec.apply_pending();
        ed.collect();
        assert_eq!(ed.mix(key(1)), Mix::new(0.75));
    }
    // `set_mix` is the same writer.
    ed.set_mix(key(1), Mix::new(0.5)).expect("a node");
    assert_eq!(knob.load(Ordering::Acquire), 0.5);
    ed.set_mix(key(1), Mix::new(0.25)).expect("a node");
    knob.store(0.875, Ordering::Release);
    // A fork starts at the base the cell holds.
    let (fed, mut fexec) = ed
        .fork(ForkTarget::Master, ForkMode::Offline, prepare(BLOCK))
        .expect("forks");
    assert_eq!(fed.mix(key(1)), Mix::new(0.875));
    let o = block(&mut fexec, &input);
    for n in 0..BLOCK {
        assert_eq!(
            o[n].to_bits(),
            blend(3.0 * input[n], input[n], 0.875).to_bits()
        );
    }
    // And its range is the mode cell's.
    let at = mix_of(1);
    let range = ed.param_range_cell(at).expect("the mix has a range cell");
    assert_eq!(range.load(), NodeMix::DEFAULT_RANGE);
    ed.set_param_range(at, ParamRange::new(-3.0, 3.0))
        .expect("set");
    assert_eq!(
        range.load(),
        ParamRange::new(0.0, 1.0),
        "narrowed to the blend"
    );
    ed.set_param_range(at, ParamRange::new(0.25, 0.5))
        .expect("set");
    assert_eq!(range.load(), ParamRange::new(0.25, 0.5));
}
