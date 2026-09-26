//! The audio-rate modulation, asserted as a **value**.
//!
//! `mod_audio_rate.rs` asserts what the reconciler *declares* and what the
//! graph *computes* against the frame-rate path. This file asserts what
//! neither says without the whole value in hand: which sources, with which
//! shapings, the graph's spec holds for a modulated param.
//!
//! It used to assert the same things about a sub-graph — which shaper fed
//! which port of a `ParamSumNode`, and a shaper's shaping lifted into its
//! node's `NodeSpec` — because the chain was nodes in the `Topology`. The
//! graph owns the arithmetic now (design doc 013 item 6), and its value holds
//! the modulation directly (`GraphSpec::params`, read here through
//! `AudioGraphRes::param_mod`): one entry per modulated param, each source
//! with its shaping. Every property below is the old one, asked of that
//! value.

#![cfg(feature = "modulation")]

#[macro_use]
mod common;

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;

use bevy_tutti::graph::{AudioGraphRes, CapturedControls, GraphReconcilePlugin};
use bevy_tutti::modulation::audio_rate::ModSourceNode;
use bevy_tutti::modulation::{
    ModParamRange, ModRoute, ModSource, ModSourceRate, ModTargetRegistry, TuttiModulationPlugin,
};
use bevy_tutti::AudioEngineState;
use tutti_core::AudioNode;
use tutti_graph::{ParamFrom, ParamMod};
use tutti_mod::LfoShape;
use tutti_nodes::{DistortionNode, ShapeKind};
use tutti_types::graph::{NodeKey, OutPort};
use tutti_types::{Depth, Hz, ParamAddr, UnitParam};

/// An app with the engine's plugins and one distortion, ready to modulate.
/// Mirrors `mod_audio_rate.rs`'s fixture so the two files describe the same
/// graph.
fn app_with_target() -> (App, Entity) {
    let mut app = App::new();
    app.insert_resource(AudioGraphRes::headless(0, 2));
    app.insert_resource(AudioEngineState::Running);
    app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));
    app.world_mut()
        .resource_mut::<ModTargetRegistry>()
        .register::<DistortionNode>();

    let dist = DistortionNode::new(ShapeKind::Tanh, 5.0);
    // Its controls, captured from the unit before it moves — the step every
    // insertion path in `bevy_tutti::graph` runs.
    let controls = CapturedControls::capture(app.world(), &dist);
    let node = app.world_mut().resource_mut::<AudioGraphRes>().insert(dist);
    let mut target = app.world_mut().spawn(ModParamRange::default().with(
        ParamAddr::Unit(UnitParam::Drive),
        5.0,
        0.0,
        10.0,
    ));
    controls.bind(&mut target, node);
    let target = target.id();
    (app, target)
}

fn spawn_lfo(app: &mut App) -> Entity {
    app.world_mut()
        .spawn((
            ModSource::new(LfoShape::Sine),
            ModSourceRate::free_running(Hz(2.0)),
        ))
        .id()
}

/// The graph value's modulation of `target`'s drive.
fn drive_mod(app: &App, target: Entity) -> Option<ParamMod> {
    let node = *app.world().get::<AudioNode>(target).expect("AudioNode");
    app.world()
        .resource::<AudioGraphRes>()
        .param_mod(node, UnitParam::Drive)
}

/// `lfo`'s node's output 0, as a param source.
fn source_of(app: &App, lfo: Entity) -> ParamFrom {
    let e = app
        .world()
        .get::<ModSourceNode>(lfo)
        .expect("the audio-rate tier gave the source a node")
        .0;
    let node = *app.world().get::<AudioNode>(e).expect("AudioNode");
    ParamFrom::Audio(OutPort {
        node: NodeKey(node.0.value()),
        port: 0,
    })
}

fn shaping(depth: f32, curve: tutti_mod::CurveType) -> tutti_graph::ParamShaping {
    tutti_nodes::ParamModShaping {
        depth: Depth(depth),
        polarity: tutti_mod::Polarity::Bipolar,
        curve,
    }
    .shaping()
}

/// **Every route in a group is its own source.**
///
/// The hole this closed for the chain: collapsing all N shapers onto one sum
/// port passed the suite, since nothing asserted the *set* of edges into the
/// sum — two routes at depth 0.25 became one, silently. Asked of the value
/// now: the modulation holds exactly one source per route, each the route's
/// own node.
///
/// Mutation (run): declare every route of a group with the first route's
/// source (`group[0].source` for `r.source`) → the sources collapse to one
/// → fails.
#[test]
fn every_route_in_a_group_is_its_own_source() {
    let (mut app, target) = app_with_target();
    let sources: Vec<Entity> = (0..3).map(|_| spawn_lfo(&mut app)).collect();

    for source in &sources {
        app.world_mut().spawn(
            ModRoute::new(*source, target, ParamAddr::Unit(UnitParam::Drive))
                .with_depth(Depth(0.25))
                .per_sample(),
        );
    }
    // Two updates: the first spawns the source nodes, the second declares the
    // modulation once they are visible.
    app.update();
    app.update();

    let m = drive_mod(&app, target).expect("the group is in the value");
    let mut got: Vec<ParamFrom> = m.sources.iter().map(|s| s.from).collect();
    let mut want: Vec<ParamFrom> = sources.iter().map(|&s| source_of(&app, s)).collect();
    got.sort();
    want.sort();
    assert_eq!(
        got, want,
        "each route is its own source; a collapse would keep fewer, and only the \
         whole source set shows it"
    );
}

/// Despawning a route's **source node** leaves no stale source in the value:
/// the graph drops the node's param edges with it, and the rest of the group
/// keeps modulating.
///
/// Two mechanisms hold this, redundantly by design: the graph drops a removed
/// node's param edges (`Editor::remove`), and the reconciler redeclares the
/// group without the departed source (`RemovedComponents<AudioNode>` in its
/// gate). Mutation (run): remove both — `Editor::remove` keeping sources from
/// the key, and the gate's `unbound` — → a stale source stays → fails. Either
/// alone passes, being covered by the other; `tutti-graph`'s
/// `removing_a_node_drops_its_param_edges` pins the editor's half by itself.
#[test]
fn despawning_a_mod_source_leaves_no_stale_source_in_the_value() {
    let (mut app, target) = app_with_target();
    let (a, b) = (spawn_lfo(&mut app), spawn_lfo(&mut app));

    for source in [a, b] {
        app.world_mut().spawn(
            ModRoute::new(source, target, ParamAddr::Unit(UnitParam::Drive))
                .with_depth(Depth(0.25))
                .per_sample(),
        );
    }
    app.update();
    app.update();
    assert_eq!(drive_mod(&app, target).expect("the group").sources.len(), 2);

    let gone = source_of(&app, a);
    let kept = source_of(&app, b);
    let source_node = app.world().get::<ModSourceNode>(a).expect("a node").0;
    app.world_mut().entity_mut(source_node).despawn();
    app.update();

    let m = drive_mod(&app, target).expect("the other source still modulates");
    assert!(
        m.sources.iter().all(|s| s.from != gone),
        "the despawned node's source left the value"
    );
    assert_eq!(
        m.sources.iter().map(|s| s.from).collect::<Vec<_>>(),
        vec![kept],
        "and the rest of the group stayed"
    );
}

/// **The value carries the shaping each source is rendered with** — the
/// table the graph's fused step reads, built from the route's depth,
/// polarity and curve. What used to be lifted from a shaper entity's
/// `ShaperShaping` into its `NodeSpec` by hand is now simply the value.
///
/// Mutation (run): declare every source with depth 1 (ignore `r.depth`) →
/// fails.
#[test]
fn the_value_carries_each_sources_shaping() {
    let (mut app, target) = app_with_target();
    let lfo = spawn_lfo(&mut app);
    app.world_mut().spawn(
        ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive))
            .with_depth(Depth(0.1))
            .per_sample(),
    );
    app.update();
    app.update();

    let m = drive_mod(&app, target).expect("the modulation");
    assert_eq!(
        m.sources[0].shaping,
        shaping(0.1, tutti_mod::CurveType::Linear),
        "the value must carry the shaping the route asks for; without it nothing \
         could tell two depths apart"
    );
}

/// Two shapings that differ **only** in curve are different values.
///
/// A depth edit moves a scalar, which almost any encoding would catch. A curve
/// moves the whole table, and an encoding that lost it (a hash collision, a
/// dropped `Bezier` payload) would let a curve edit compare equal and be
/// skipped. The value compares the tables' bits.
///
/// Mutation (run): build every shaping with `CurveType::Linear` → fails.
#[test]
fn a_curve_only_difference_is_visible_in_the_value() {
    let shaping_for = |curve: tutti_mod::CurveType| {
        let (mut app, target) = app_with_target();
        let lfo = spawn_lfo(&mut app);
        let mut route = ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive))
            .with_depth(Depth(0.5))
            .per_sample();
        route.curve = curve;
        app.world_mut().spawn(route);
        app.update();
        app.update();
        drive_mod(&app, target).expect("the modulation").sources[0]
            .shaping
            .clone()
    };

    let linear = shaping_for(tutti_mod::CurveType::Linear);
    let exponential = shaping_for(tutti_mod::CurveType::Exponential);
    assert_eq!(linear, shaping(0.5, tutti_mod::CurveType::Linear));
    assert_ne!(
        linear, exponential,
        "two curves must not share a value; a collision would make a curve edit \
         invisible and the redeclaration would be skipped"
    );
}

/// **An unchanged route is not redeclared.**
///
/// The other half of "did the shaping move". A reconciler that redeclared
/// every frame would recompile the graph every frame and restart the
/// connection's declick each time. Asserted on the value and on the graph's
/// dirty flag rather than on sound.
///
/// Mutation (run): make the reconciler's comparison always report a change
/// (`old == Some(&p)` never true) → the graph is dirtied on a frame that
/// changed nothing → fails.
#[test]
fn an_unchanged_route_is_not_redeclared() {
    let (mut app, target) = app_with_target();
    let lfo = spawn_lfo(&mut app);
    let route = app
        .world_mut()
        .spawn(
            ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive))
                .with_depth(Depth(0.1))
                .per_sample(),
        )
        .id();
    app.update();
    app.update();
    let before = drive_mod(&app, target).expect("the modulation");

    // What the reconciler left the graph's dirty flag at, read right after
    // it runs (the frame's commit clears it later).
    #[derive(Resource, Default)]
    struct SeenDirty(bool);
    fn record(dirty: Res<bevy_tutti::graph::GraphDirty>, mut seen: ResMut<SeenDirty>) {
        seen.0 = dirty.0;
    }
    app.init_resource::<SeenDirty>().add_systems(
        Update,
        record
            .after(bevy_tutti::modulation::audio_rate::reconcile_audio_rate)
            .in_set(bevy_tutti::graph::GraphReconcileSystems::Spawn),
    );

    // Touch the route without changing it: the reconciler runs, and must
    // find nothing to do.
    app.world_mut().get_mut::<ModRoute>(route).unwrap().depth = Depth(0.1);
    app.update();

    assert_eq!(
        drive_mod(&app, target).expect("the modulation"),
        before,
        "the value is unchanged"
    );
    assert!(
        !app.world().resource::<SeenDirty>().0,
        "and nothing was redeclared: an unchanged route must not recompile the graph"
    );
}

/// **A changed shaping reaches the value.**
///
/// A missed change is a slider that moves on screen and not in the sound.
///
/// Mutation (run): compare only sources' nodes when deciding what changed
/// (ignore shapings) → fails.
#[test]
fn a_changed_shaping_reaches_the_value() {
    let (mut app, target) = app_with_target();
    let lfo = spawn_lfo(&mut app);
    let route = app
        .world_mut()
        .spawn(
            ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive))
                .with_depth(Depth(0.1))
                .per_sample(),
        )
        .id();
    app.update();
    app.update();

    app.world_mut().get_mut::<ModRoute>(route).unwrap().depth = Depth(0.8);
    app.update();

    assert_eq!(
        drive_mod(&app, target).expect("the modulation").sources[0].shaping,
        shaping(0.8, tutti_mod::CurveType::Linear),
        "the value must describe the shaping that now renders, not the one it \
         replaced"
    );
}
