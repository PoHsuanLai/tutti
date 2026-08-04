//! What would it take for the modulation matrix to emit an audio-rate chain?
//!
//! The matrix currently delivers every native-param route at frame rate (an
//! `AtomicTarget` mirroring into the node's atomic). The per-sample tier exists
//! — `ParamShaperUnit → ParamSumUnit → node.param_port` — but nothing connects a
//! `ModRoute` to it.
//!
//! These tests pin the translation, so the eventual reconciler has a spec rather
//! than an intention: **every field the chain needs is already on `ModRoute`**,
//! and the chain it produces agrees with the frame-rate path it replaces.

#![cfg(feature = "modulation")]

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;

use bevy_tutti::graph::{
    AudioGraphRes, AudioSource, AudioSources, GraphReconcilePlugin, SpawnAudioNode,
};
use bevy_tutti::modulation::{ModRoute, ModSource};
use bevy_tutti::AudioEngineState;
use tutti_core::dsp::{AudioUnit as _, Net, Source};
use tutti_core::AudioNode;
use tutti_mod::{shape, CurveType, LfoShape, Polarity};
use tutti_types::{Depth, ParamAddr, UnitParam};
use tutti_units::{
    AtomicSourceUnit, DistortionNode, ParamPorts, ParamShaperUnit, ParamSumUnit, ShapeKind,
};

fn app() -> App {
    let mut app = App::new();
    app.insert_resource(AudioGraphRes(Net::with_backend(2)));
    app.insert_resource(AudioEngineState::Running);
    app.add_plugins(GraphReconcilePlugin);
    app
}

fn node_id(app: &App, entity: Entity) -> tutti_core::NodeId {
    app.world().get::<AudioNode>(entity).expect("AudioNode").0
}

/// **The translation is total.** Every input `ParamShaperUnit::new` needs is a
/// field already on `ModRoute` — no new authoring vocabulary, no new component.
///
/// This is the thing that makes the reconciler mechanical rather than a design:
/// the route a user already writes for frame-rate modulation carries exactly the
/// data the audio-rate chain wants.
#[test]
fn a_mod_route_carries_everything_the_chain_needs() {
    let mut world = World::new();
    let (src, dst) = (world.spawn_empty().id(), world.spawn_empty().id());

    let route = ModRoute::new(src, dst, ParamAddr::Unit(UnitParam::Drive))
        .with_depth(Depth(0.5))
        .with_polarity(Polarity::Unipolar)
        .with_curve(CurveType::Exponential);

    // The shaper is built straight from the route's fields.
    let shaper = ParamShaperUnit::new(route.depth, route.polarity, route.curve);

    // ...and it agrees with the control-rate shaping of the same route, which is
    // what keeps a route's sound stable if delivery ever switches tiers.
    for x in [-1.0f32, -0.5, 0.0, 0.5, 1.0] {
        let mut got = [0.0f32; 1];
        let mut s = shaper.clone();
        s.tick(&[x], &mut got);
        let want = shape(x, route.depth, route.polarity, route.curve);
        assert!(
            (got[0] - want).abs() < 1e-3,
            "audio-rate shaping diverged from the route's control-rate shaping \
             at {x}: {} vs {want}",
            got[0]
        );
    }
}

/// The shape of what a reconciler would emit, spelled out end to end.
///
/// Written by hand here because no reconciler exists yet — this *is* the spec
/// for one. Note what it needs that the matrix does not currently track: the
/// target's param-port index, and one `ParamSumUnit` sized to the number of
/// routes landing on that param.
#[test]
fn the_chain_a_reconciler_would_emit() {
    let mut app = app();

    // --- what the user authored ---
    let lfo = app.world_mut().spawn(ModSource::new(LfoShape::Sine)).id();

    let dist = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 5.0, true);
    let drive_port = dist.param_port(UnitParam::Drive).expect("drive port");

    let target = {
        let mut commands = app.world_mut().commands();
        let e = commands.spawn_audio_node(dist).id();
        e
    };
    app.world_mut().flush();

    let route =
        ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.5));
    app.world_mut().spawn(route);

    // --- what a reconciler would build from it ---
    // One base per modulated param, one sum sized to the routes landing on it,
    // one shaper per route.
    let (base, sum, shaper) = {
        let mut commands = app.world_mut().commands();
        let base = commands.spawn_audio_node(AtomicSourceUnit::new(5.0)).id();
        let sum = commands
            .spawn_audio_node(ParamSumUnit::new(1, 0.0, 10.0))
            .id();
        let shaper = commands
            .spawn_audio_node(ParamShaperUnit::new(
                route.depth,
                route.polarity,
                route.curve,
            ))
            .id();
        (base, sum, shaper)
    };
    app.world_mut().flush();

    // The source feeding the shaper would be the LFO's *node* — the matrix has
    // no node for a ModSource today (it builds a `tutti_mod::Modulator`, not an
    // AudioUnit), which is the one genuinely missing piece. Stand in with a
    // constant so the wiring is still assertable.
    let source_node = {
        let mut commands = app.world_mut().commands();
        commands.spawn_audio_node(AtomicSourceUnit::new(1.0)).id()
    };
    app.world_mut().flush();

    app.world_mut()
        .entity_mut(shaper)
        .insert(AudioSources::from(source_node));
    app.world_mut().entity_mut(sum).insert(
        AudioSources::silent()
            .with(0, AudioSource::node(base))
            .with(1, AudioSource::node(shaper)),
    );
    app.world_mut()
        .entity_mut(target)
        .insert(AudioSources::silent().with(drive_port, AudioSource::node(sum)));
    app.update();

    // --- the graph the engine actually holds ---
    let (t, s, b, sh, src) = (
        node_id(&app, target),
        node_id(&app, sum),
        node_id(&app, base),
        node_id(&app, shaper),
        node_id(&app, source_node),
    );
    let graph = app.world().resource::<AudioGraphRes>();

    assert_eq!(
        graph.0.source(sh, 0),
        Source::Local(src, 0),
        "source → shaper"
    );
    assert_eq!(
        graph.0.source(s, 0),
        Source::Local(b, 0),
        "base → sum port 0"
    );
    assert_eq!(
        graph.0.source(s, 1),
        Source::Local(sh, 0),
        "shaper → sum port 1"
    );
    assert_eq!(
        graph.0.source(t, drive_port),
        Source::Local(s, 0),
        "sum → the node's param port"
    );
}

/// Two routes onto one param share **one** sum, sized to hold both — the
/// audio-rate mirror of the distinct-layer-keys rule the frame-rate path uses
/// to make two routes sum instead of overwrite.
///
/// This is why the reconciler has to group routes by `(target, param)` before
/// building anything: the sum's arity is a function of the group, not of any
/// single route.
#[test]
fn two_routes_onto_one_param_share_one_sum() {
    let mut app = app();

    let dist = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 5.0, true);
    let drive_port = dist.param_port(UnitParam::Drive).unwrap();
    let target = {
        let mut c = app.world_mut().commands();
        c.spawn_audio_node(dist).id()
    };
    app.world_mut().flush();

    // Two routes → a sum with 2 offset ports (1 + 2 inputs total).
    let (base, sum, a, b) = {
        let mut c = app.world_mut().commands();
        let base = c.spawn_audio_node(AtomicSourceUnit::new(5.0)).id();
        let sum = c.spawn_audio_node(ParamSumUnit::new(2, 0.0, 10.0)).id();
        let a = c
            .spawn_audio_node(ParamShaperUnit::new(
                Depth(0.25),
                Polarity::Bipolar,
                CurveType::Linear,
            ))
            .id();
        let b = c
            .spawn_audio_node(ParamShaperUnit::new(
                Depth(0.25),
                Polarity::Bipolar,
                CurveType::Linear,
            ))
            .id();
        (base, sum, a, b)
    };
    app.world_mut().flush();

    app.world_mut().entity_mut(sum).insert(
        AudioSources::silent()
            .with(0, AudioSource::node(base))
            .with(1, AudioSource::node(a))
            .with(2, AudioSource::node(b)),
    );
    app.world_mut()
        .entity_mut(target)
        .insert(AudioSources::silent().with(drive_port, AudioSource::node(sum)));
    app.update();

    let (s, sa, sb) = (node_id(&app, sum), node_id(&app, a), node_id(&app, b));
    let graph = app.world().resource::<AudioGraphRes>();
    assert_eq!(graph.0.source(s, 1), Source::Local(sa, 0));
    assert_eq!(graph.0.source(s, 2), Source::Local(sb, 0));
    assert_eq!(
        graph.0.inputs_in(s),
        3,
        "one base port plus one offset port per route"
    );
}

/// The arithmetic the chain performs, verified against the same `fold` the
/// frame-rate accumulator uses. Two full-scale sources at depth 0.25 land the
/// same offset whichever tier evaluates them.
#[test]
fn the_chain_sums_to_what_the_frame_rate_path_would() {
    let depth = Depth(0.25);
    let (base, min, max) = (5.0f32, 0.0f32, 10.0f32);

    // Audio-rate: two shapers into a sum.
    let mut sum = ParamSumUnit::new(2, min, max);
    let shaped = shape(1.0, depth, Polarity::Bipolar, CurveType::Linear);
    let mut out = [0.0f32; 1];
    sum.tick(&[base, shaped, shaped], &mut out);

    // Frame-rate: the same offsets folded by tutti-mod.
    let want = tutti_mod::fold(base, [shaped, shaped].into_iter(), min, max);

    assert!(
        (out[0] - want).abs() < 1e-6,
        "audio-rate sum {} disagrees with the control-rate fold {want}",
        out[0]
    );
}
