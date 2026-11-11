//! A modulation route onto the **mix of a node** (doc 013, "Bypass and mute
//! are graph state", M4b): `Mods::route(source, MixRef, depth)` gives the node
//! a driven mix lane, as a route onto a param gives a port one, and the mix
//! cell is the lane's knob (`Graph::set_mix` is the one writer).
//!
//! The figures are analytic. A constant `A` through a node that halves it: the
//! node's output is `y = A / 2`, its aligned input `x̂ = A` (the node has no
//! latency), and the render is `y * m + x̂ * (1 - m)` with `m` the lane's value
//! (a bipolar linear route: the knob, the mix `Graph::set_mix` holds, plus
//! `depth * macro * span`, clamped to the range; every figure is exact in
//! `f32`).

use tutti_graph::{Cx, ForkByClone, Io, Node, NodeKey, Prepare, Shape, Status};
use tutti_modulation::Macro;
use tutti_nodes::ReconcileError;
use tutti_nodes::testing::Const;
use tutti_session::{GraphSource, MixRef, ModRefusal, ModTarget, ParamRef, PassError, Session};
use tutti_types::{ChannelLayout, Depth, InterleavedMut, Mix, SampleRate, Samples, UnitParam};

const BLOCK: usize = 128;
/// The constant source's level.
const A: f32 = 0.25;

/// `out = in * 0.5`: a node whose output differs from its input, so the blend
/// is audible.
#[derive(Clone)]
struct Halve;

impl Node for Halve {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (ins, mut outs) = io.split();
        for (o, &x) in outs.get(0).iter_mut().zip(ins.get(0)) {
            *o = x * 0.5;
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn offline() -> Session {
    Session::builder()
        .build_offline(SampleRate(48_000.0), ChannelLayout::STEREO, Samples(1_024))
        .expect("builds")
}

fn settle(s: &mut Session) {
    for _ in 0..6 {
        s.update();
    }
    assert_eq!(s.mod_state().refusal(), None, "the declarations apply");
    assert_eq!(
        s.mod_state().unresolved_targets(),
        &[],
        "every target resolved"
    );
}

/// Output channel 0 (the left of the stereo root) for `blocks` blocks.
fn render(s: &mut Session, blocks: usize) -> Vec<f32> {
    let mut all = Vec::new();
    let mut buf = vec![0.0f32; BLOCK * 2];
    for _ in 0..blocks {
        s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
        all.extend(buf.as_chunks::<2>().0.iter().map(|f| f[0]));
    }
    all
}

/// `Const(A) -> Halve -> output 0`; the node and the render's blend at `m`.
fn rig(s: &mut Session) -> NodeKey {
    let g = s.graph();
    let (a, _) = g.insert(Const::mono(A));
    let (halve, _) = g.insert(ForkByClone(Halve));
    g.set_source(halve, 0, GraphSource::Node(a, 0));
    g.set_output_source(0, GraphSource::Node(halve, 0));
    halve
}

fn blend(m: f32) -> f32 {
    (A * 0.5) * m + A * (1.0 - m)
}

/// Every frame past the lane's first block and the connect's declick is the
/// blend at `m`.
fn figure(s: &mut Session, m: f32, what: &str) {
    let got = render(s, 12);
    for (f, &x) in got.iter().enumerate().skip(512) {
        assert_eq!(x.to_bits(), blend(m).to_bits(), "{what}, frame {f}");
    }
}

/// A route onto a node's mix is the blend at the analytic mix, and the mix has
/// one writer: `Graph::set_mix` is the knob whether or not the node is
/// routed. A mix of `0.25` stays the base when a route arrives (the lane
/// modulates around it), the route moves it by `depth * macro * span` (over the
/// range `0..=1`, macro `0.5` at depth `0.5`: `0.25 + 0.25 = 0.5`), a base moved
/// while routed moves the modulated figure (`0.5 + 0.25 = 0.75`), the macro
/// moved is the offset moved (`0.5 - 0.25 = 0.25`), a value past the range is
/// clamped (the knob written through `ModState::set_knob` to `0.875` and the
/// macro back to `0.5` is `1.125`, clamped to `1.0`), and with the route
/// removed the mix returns to the base the knob left, `0.875`, once the lane's
/// source goes (the declick), not to WET.
///
/// Mutations (each run, each failed): the lane's knob its own cell starting at
/// the unity mix (`Some(Arc::new(AtomicF32::new(1.0)).into())` for the
/// `Bind::Mix` build's knob in `tutti-nodes`) -> the routed figure is
/// `1.0 + 0.25`, clamped to `1.0` (WET), the base moved while routed is
/// ignored and the route gone reads WET; `Editor::mix_cell` answering a cell
/// the unit does not read (`ModeCell::knob` giving a fresh `Arc`) -> the knob's
/// writes reach nothing and the first figure is the plain `0.25` blend with no
/// offset; `carry_mix` storing unconditionally -> the commit after
/// `set_knob` puts the stale `0.25` back; leaving `connect_param` out of the
/// reconciler's `Bind::Mix` lane build -> the lane never reaches the node and
/// every figure is the base's.
#[test]
fn a_route_onto_a_mix_modulates_around_the_base_set_mix_writes() {
    let mut s = offline();
    let node = rig(&mut s);
    settle(&mut s);
    s.graph().set_mix(node, Mix::new(0.25)).expect("a node");
    settle(&mut s);
    figure(&mut s, 0.25, "the base, before any route");

    let mix = MixRef::of(node);
    let mut m = s.mods();
    m.range(mix, 0.0, 1.0);
    let source = m.macro_(0.5);
    let route = m.route(source, mix, Depth(0.5));
    let target = m.key(mix);
    settle(&mut s);
    figure(&mut s, 0.5, "the base plus the route's offset");

    s.graph().set_mix(node, Mix::new(0.5)).expect("a node");
    settle(&mut s);
    figure(&mut s, 0.75, "the base moved while routed");
    assert_eq!(
        s.graph_ref().mix(node),
        Mix::new(0.5),
        "the base, not the lane"
    );

    s.mods().set_source(source, Macro(-0.5));
    settle(&mut s);
    figure(&mut s, 0.25, "the macro moved");

    s.mods().set_source(source, Macro(0.5));
    assert!(s.mod_state().set_knob(target, 0.875), "the lane has a knob");
    settle(&mut s);
    figure(&mut s, 1.0, "clamped to the blend");
    assert_eq!(
        s.graph_ref().mix(node),
        Mix::new(0.875),
        "the knob is the mix"
    );

    s.mods().remove_route(route);
    settle(&mut s);
    figure(&mut s, 0.875, "the route gone: the base the knob left");
}

/// A mix target keys once per node, apart from a param's of the same node and
/// from an edge's, and is no param.
///
/// Mutation (run, failed): `ModTarget::Mix` keyed as a param (`ParamRef::key`
/// of `Volume`) -> the first assertion fails.
#[test]
fn a_mix_target_keys_apart_from_a_param_of_its_node() {
    let mut s = offline();
    let node = rig(&mut s);
    let key = s.mods().key(MixRef::of(node));
    assert_eq!(key, s.mods().key(ModTarget::Mix(MixRef::of(node))));
    assert_ne!(key, s.mods().key(ParamRef::unit(node, UnitParam::Volume)));
    assert_eq!(ParamRef::from_key(key), None);
}

/// A removed node takes the routes onto its mix with it: nothing is left
/// declared on it, so the target is not left unresolved.
///
/// Mutation (run, failed): `forget_node` not matching `TargetKey::Mix` -> the
/// route outlives its node and its target stays unresolved.
#[test]
fn a_removed_node_takes_its_mix_route_with_it() {
    let mut s = offline();
    let node = rig(&mut s);
    let mix = MixRef::of(node);
    let mut m = s.mods();
    m.range(mix, 0.0, 1.0);
    let source = m.macro_(0.5);
    m.route(source, mix, Depth(0.5));
    settle(&mut s);
    assert!(s.remove(node));
    // Nothing is left declared on the node: no refusal, nothing unresolved.
    settle(&mut s);
}

/// `out = (in, in)`: a mono node with a stereo output that names no placement,
/// so its dry path would be a guess (by index, the input on the left alone).
#[derive(Clone)]
struct Spread;

impl Node for Spread {
    fn shape(&self) -> Shape {
        Shape::buses(
            tutti_graph::Buses::unassigned(ChannelLayout::MONO),
            tutti_graph::Buses::unassigned(ChannelLayout::STEREO),
        )
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (ins, mut outs) = io.split();
        for c in 0..2 {
            outs.get(c).copy_from_slice(ins.get(0));
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// **A route onto the mix of a node with no pass is refused**, naming the
/// target and what the node would need to declare, as `Graph::set_mix` refuses
/// the same mix: the lane would blend in a dry signal the graph would have to
/// guess. The node itself runs.
///
/// Mutation (run): the reconciler's `Target::Mix` arm not asking
/// `Editor::check_pass` → the lane is built and the declarations apply → the
/// refusal is `None` and the assertion fails.
#[test]
fn a_route_onto_the_mix_of_a_node_with_no_pass_is_refused() {
    let mut s = offline();
    let g = s.graph();
    let (a, _) = g.insert(Const::mono(A));
    let (spread, _) = g.insert(ForkByClone(Spread));
    g.set_source(spread, 0, GraphSource::Node(a, 0));
    g.set_output_source(0, GraphSource::Node(spread, 0));
    settle(&mut s);
    assert!(
        s.graph().set_mix(spread, Mix::new(0.5)).is_err(),
        "set_mix refuses the same mix"
    );

    let mix = MixRef::of(spread);
    let mut m = s.mods();
    m.range(mix, 0.0, 1.0);
    let source = m.macro_(0.5);
    m.route(source, mix, Depth(0.5));
    let target = m.key(mix);
    for _ in 0..6 {
        s.update();
    }
    assert_eq!(
        s.mod_state().refusal(),
        Some(&ModRefusal::Apply(ReconcileError::Unplaced {
            target,
            error: PassError::Unplaced {
                inputs: 1,
                outputs: 2
            },
        }))
    );
}
