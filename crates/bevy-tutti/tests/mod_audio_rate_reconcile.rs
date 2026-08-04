//! The audio-rate reconciler: a `ModRoute` marked `at_audio_rate` becomes a
//! real graph chain, and stops being one when the route goes away.

#![cfg(feature = "modulation")]

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

/// **A route declared before its sink's node still reaches audio rate.**
///
/// The ordering a real host produces, and the one the reconciler used to fail
/// on. `spawn_chain` resolves the sink's param port through its `AudioNode`, so
/// a route whose sink has no node yet correctly builds nothing — but the gate
/// watched only `Changed<ModRoute>`/`Changed<ModParamRange>`, so when the node
/// arrived nothing asked again and the route stayed on the per-frame fallback
/// **permanently**.
///
/// This is the ordinary order, not a contrived one. A host compiling a document
/// declares routes and spawns nodes in the same frame, and `insert_audio_node`
/// lands as a *deferred* command — so the route is visible one frame before the
/// `AudioNode` is.
///
/// It failed silently, which is why it needed a test rather than a review: the
/// per-frame fallback is a legal outcome meaning "this sink exposes no port",
/// and nothing distinguishes it from "the node had not arrived yet".
#[test]
fn a_route_declared_before_its_sinks_node_still_binds() {
    let mut app = App::new();
    app.insert_resource(AudioGraphRes(Net::with_backend(2)));
    app.insert_resource(AudioEngineState::Running);
    app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));
    app.world_mut()
        .resource_mut::<ModTargetRegistry>()
        .register::<DistortionNode>();

    // The sink exists as an entity with its declared range, but carries **no**
    // `AudioNode` yet — exactly what a projection produces before the spawner
    // has run.
    let target = app
        .world_mut()
        .spawn(ModParamRange::default().with(ParamAddr::Unit(UnitParam::Drive), 5.0, 0.0, 10.0))
        .id();
    let lfo = spawn_lfo(&mut app);
    app.world_mut().spawn(
        ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive))
            .with_depth(Depth(0.5))
            .per_sample(),
    );

    app.update();
    app.update();
    assert!(
        app.world()
            .resource::<AudioRateChains>()
            .get(target, ParamAddr::Unit(UnitParam::Drive))
            .is_none(),
        "with no node on the sink there is no port to resolve, so no chain — \
         this half must hold or the assertion below proves nothing"
    );

    // The node arrives a frame later, as a deferred insert would.
    let dist = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 5.0, true);
    let drive_port = dist.param_port(UnitParam::Drive).unwrap();
    let node = app.world_mut().resource_mut::<AudioGraphRes>().0.add(dist);
    app.world_mut().entity_mut(target).insert(AudioNode(node));

    app.update();
    app.update();

    let chain = app
        .world()
        .resource::<AudioRateChains>()
        .get(target, ParamAddr::Unit(UnitParam::Drive))
        .expect(
            "the sink's node arrived, so the route must now bind — a node \
             appearing after its route is the ordinary order, not an edge case",
        )
        .clone();
    assert_eq!(chain.port, drive_port);
}

/// **Editing a modulated param's authored range must not delete its modulation.**
///
/// `rebuild` runs when a route *or a range* changes, and builds its source
/// registry from `CollectedModSources::sources` — which it **drains**. But the
/// per-kind `collect` systems refill that list only when `collected.dirty`, and
/// `dirty` tracks *source* changes. So a rebuild triggered by a range change
/// alone found an empty registry, failed `source_index.get(..)` for every route,
/// and dropped every accumulator.
///
/// The user-visible effect: turning the knob on a modulated parameter silently
/// removes its LFO. Nothing errors — the matrix simply empties.
///
/// A range change is not exotic. `ModParamRange` carries the authored
/// `base`/`min`/`max`, so any host that mirrors an authored value into it
/// re-inserts the component on every edit.
#[test]
fn editing_a_range_does_not_drop_the_routes() {
    let (mut app, target, _port) = app_with_target();
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
        "the route must bind first, or the assertion below is vacuous"
    );

    // Re-declare the range with a new base — what a host does when the user
    // moves the authored value of a modulated param.
    app.world_mut().entity_mut(target).insert(
        ModParamRange::default().with(ParamAddr::Unit(UnitParam::Drive), 7.0, 0.0, 10.0),
    );
    app.update();

    assert!(
        app.world()
            .resource::<bevy_tutti::modulation::ModulationMatrix>()
            .is_modulated(target, ParamAddr::Unit(UnitParam::Drive)),
        "the route must survive a range edit — a rebuild that drains its source \
         registry without refilling it drops every route it cannot resolve"
    );
}
