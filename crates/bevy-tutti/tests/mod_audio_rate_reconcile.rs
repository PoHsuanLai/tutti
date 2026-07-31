//! The audio-rate reconciler: a `ModRoute` marked `at_audio_rate` becomes a
//! real graph chain, and stops being one when the route goes away.

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;

use bevy_tutti::graph::{AudioGraphRes, GraphReconcilePlugin};
use bevy_tutti::modulation::audio_rate::{AudioRateChains, ModSourceNode};
use bevy_tutti::modulation::{
    ModParamRange, ModRate, ModRoute, ModSource, ModTargetRegistry, TuttiModulationPlugin,
};
use bevy_tutti::AudioEngineState;
use tutti_core::dsp::{Net, Source};
use tutti_core::AudioNode;
use tutti_mod::LfoShape;
use tutti_types::{Depth, Hz, ParamAddr, UnitParam};
use tutti_units::{DistortionNode, ParamPorts, ShapeKind};

/// An app with the engine's plugins and one ported distortion, ready to modulate.
fn app_with_target() -> (App, Entity, usize) {
    let mut app = App::new();
    app.insert_resource(AudioGraphRes(Net::with_backend(2)));
    app.insert_resource(AudioEngineState::Running);
    app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));
    app.world_mut()
        .resource_mut::<ModTargetRegistry>()
        .register::<DistortionNode>();

    // Born with its drive port on — the trigger policy this crate settled on.
    let dist = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 5.0, true);
    let drive_port = dist.param_port(UnitParam::Drive).unwrap();
    let node = app.world_mut().resource_mut::<AudioGraphRes>().0.add(dist);

    let target = app
        .world_mut()
        .spawn((
            AudioNode(node),
            ModParamRange::default().with(ParamAddr::Unit(UnitParam::Drive), 5.0, 0.0, 10.0),
        ))
        .id();

    (app, target, drive_port)
}

fn spawn_lfo(app: &mut App) -> Entity {
    app.world_mut()
        .spawn((
            ModSource::new(LfoShape::Sine),
            ModRate::free_running(Hz(2.0)),
        ))
        .id()
}

fn node_id(app: &App, entity: Entity) -> tutti_core::NodeId {
    app.world().get::<AudioNode>(entity).expect("AudioNode").0
}

/// The headline: an `at_audio_rate` route materialises
/// `source → shaper → sum → param port`, entirely from the declaration.
#[test]
fn an_audio_rate_route_builds_the_whole_chain() {
    let (mut app, target, drive_port) = app_with_target();
    let lfo = spawn_lfo(&mut app);

    app.world_mut().spawn(
        ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive))
            .with_depth(Depth(0.5))
            .per_sample(),
    );
    // Two updates: the first spawns source nodes and the chain, the second lets
    // the wire reconciler see the declarations they inserted.
    app.update();
    app.update();

    let chains = app.world().resource::<AudioRateChains>();
    let chain = chains
        .get(target, ParamAddr::Unit(UnitParam::Drive))
        .expect("a chain was built for the modulated param")
        .clone();

    assert_eq!(chain.shapers.len(), 1, "one shaper per route");
    assert_eq!(chain.port, drive_port);

    // The source gained a renderable node — the piece the value path never
    // needed.
    let source_node = app
        .world()
        .get::<ModSourceNode>(lfo)
        .expect("the source gained an LfoNode")
        .0;

    let (t, sum, base, shaper, src) = (
        node_id(&app, target),
        node_id(&app, chain.sum),
        node_id(&app, chain.base),
        node_id(&app, chain.shapers[0]),
        node_id(&app, source_node),
    );
    let graph = app.world().resource::<AudioGraphRes>();

    assert_eq!(
        graph.0.source(shaper, 0),
        Source::Local(src, 0),
        "lfo → shaper"
    );
    assert_eq!(
        graph.0.source(sum, 0),
        Source::Local(base, 0),
        "base → sum.0"
    );
    assert_eq!(
        graph.0.source(sum, 1),
        Source::Local(shaper, 0),
        "shaper → sum.1"
    );
    assert_eq!(
        graph.0.source(t, drive_port),
        Source::Local(sum, 0),
        "sum → the node's drive port"
    );
}

/// Two routes on one param share one sum, sized to the group — the constraint
/// that forces grouping by `(target, param)` before anything is spawned.
#[test]
fn two_routes_on_one_param_share_one_sum() {
    let (mut app, target, _) = app_with_target();
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

    let chains = app.world().resource::<AudioRateChains>();
    let chain = chains
        .get(target, ParamAddr::Unit(UnitParam::Drive))
        .expect("chain")
        .clone();

    assert_eq!(chain.shapers.len(), 2, "one shaper per route, one sum");

    let sum = node_id(&app, chain.sum);
    let graph = app.world().resource::<AudioGraphRes>();
    assert_eq!(
        graph.0.inputs_in(sum),
        3,
        "the sum is sized to the group: one base port plus one per route"
    );
}

/// Deleting the route tears the chain down. Without this a removed route leaves
/// a sum feeding a stale offset into the node forever — the audio-rate mirror of
/// the layer-clearing the value path does.
#[test]
fn removing_the_route_retires_the_chain() {
    let (mut app, target, _) = app_with_target();
    let lfo = spawn_lfo(&mut app);

    let route = app
        .world_mut()
        .spawn(
            ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive))
                .with_depth(Depth(0.5))
                .per_sample(),
        )
        .id();
    app.update();
    app.update();

    let key = (target, ParamAddr::Unit(UnitParam::Drive));
    assert!(app
        .world()
        .resource::<AudioRateChains>()
        .0
        .contains_key(&key));

    app.world_mut().entity_mut(route).despawn();
    app.update();

    assert!(
        !app.world()
            .resource::<AudioRateChains>()
            .0
            .contains_key(&key),
        "the chain must be retired with its route"
    );
}

/// A route left at the default (value path) builds nothing. Audio rate is
/// opt-in, because it costs two idle graph nodes per modulated param.
#[test]
fn a_value_path_route_builds_no_chain() {
    let (mut app, target, _) = app_with_target();
    let lfo = spawn_lfo(&mut app);

    // No `.per_sample()`.
    app.world_mut().spawn(
        ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.5)),
    );
    app.update();
    app.update();

    assert!(
        app.world().resource::<AudioRateChains>().0.is_empty(),
        "the value path must not spawn graph nodes"
    );
    assert!(
        app.world().get::<ModSourceNode>(lfo).is_none(),
        "and its source must not gain a node it does not need"
    );
}

/// **The bug the enum exists to prevent.**
///
/// A per-sample route is delivered as a graph chain feeding the sink's param
/// port. If `rebuild` *also* gave it a `ModEdge`, the driver would flush
/// `base + Σ offsets` into the node's atomic every frame while the sum drove its
/// port — two writers over one param.
///
/// With the old independent bools this was not merely possible but the default:
/// `at_audio_rate` was invisible to `rebuild`, so every audio-rate route got
/// both. `ModDelivery` makes the tiers mutually exclusive by construction, and
/// this pins the driver actually honouring that.
#[test]
fn a_per_sample_route_is_not_also_claimed_by_the_driver() {
    let (mut app, target, _) = app_with_target();
    let lfo = spawn_lfo(&mut app);

    app.world_mut().spawn(
        ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive))
            .with_depth(Depth(0.5))
            .per_sample(),
    );
    app.update();
    app.update();

    // The chain exists...
    assert!(
        app.world()
            .resource::<AudioRateChains>()
            .is_audio_rate(target, ParamAddr::Unit(UnitParam::Drive)),
        "the per-sample chain must be built"
    );
    // ...and the frame-rate driver has NOT claimed the same param.
    assert!(
        !app.world()
            .resource::<bevy_tutti::modulation::ModulationMatrix>()
            .is_modulated(target, ParamAddr::Unit(UnitParam::Drive)),
        "the driver must not also own a param delivered per sample — that is \
         two writers on one atomic"
    );
}

/// The complement: a per-frame route *is* the driver's, and builds no chain.
/// Together these pin the two tiers as mutually exclusive in both directions.
#[test]
fn a_per_frame_route_is_the_drivers_alone() {
    let (mut app, target, _) = app_with_target();
    let lfo = spawn_lfo(&mut app);

    app.world_mut().spawn(
        ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.5)),
    );
    app.update();
    app.update();

    assert!(
        app.world()
            .resource::<bevy_tutti::modulation::ModulationMatrix>()
            .is_modulated(target, ParamAddr::Unit(UnitParam::Drive)),
        "the driver owns a per-frame param"
    );
    assert!(
        app.world().resource::<AudioRateChains>().0.is_empty(),
        "and no graph chain is built for it"
    );
}
