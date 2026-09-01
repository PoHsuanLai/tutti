//! The modulation adapter driven end-to-end: a real `App`, a real graph, a real
//! node, and the node's own atomic checked for movement.
//!
//! The declaration → matrix → node-atomic path is the whole point of the layer,
//! and it is the part a unit test of any single piece would miss. Every test
//! here asserts on the value the DSP actually reads.

#![cfg(feature = "modulation")]

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;

use bevy_tutti::graph::{AudioGraphRes, GraphReconcilePlugin, TransportRes};
use bevy_tutti::modulation::{
    LfoShape, ModParamRange, ModRoute, ModSource, ModSourceRate, ModTargetRegistry,
    ModulationMatrix, TuttiModulationPlugin,
};
use bevy_tutti::AudioEngineState;
use tutti_core::dsp::Net;
use tutti_core::transport::Transport;
use tutti_core::AudioNode;
use tutti_nodes::DistortionNode;
use tutti_types::{Depth, Hz, ParamAddr, UnitParam};

/// The drive an unmodulated node holds — its constructor argument, and what the
/// atomic must still read when nothing routes to it.
const UNMODULATED_DRIVE: f32 = 1.0;

/// A `Drive`-modulatable node whose param atomic we can read back.
fn drive_node() -> DistortionNode {
    DistortionNode::new(tutti_nodes::ShapeKind::Tanh, UNMODULATED_DRIVE)
}

/// An app with the reconcile pipeline, a live graph, and modulation — the same
/// wiring a host gets, minus the audio device.
fn app_with_graph() -> (App, Entity) {
    let mut app = App::new();

    let mut net = Net::new(0, 1);
    let node = net.push(Box::new(drive_node()));
    net.pipe_output(node);

    app.insert_resource(AudioGraphRes(net));
    app.insert_resource(TransportRes(Transport::new(48_000.0)));
    // The systems are gated on a running engine; nothing here opens a device,
    // so the state stands in for one.
    app.insert_resource(AudioEngineState::Running);
    app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));

    app.world_mut()
        .resource_mut::<ModTargetRegistry>()
        .register::<DistortionNode>();

    let target = app.world_mut().spawn(AudioNode(node)).id();
    (app, target)
}

/// The node's live drive value — what the DSP reads, not what the matrix thinks.
fn node_drive(app: &App, entity: Entity) -> f32 {
    let node = app.world().get::<AudioNode>(entity).unwrap().0;
    let graph = app.world().resource::<AudioGraphRes>();
    graph
        .0
        .node_as::<DistortionNode>(node)
        .unwrap()
        .drive()
        .load(std::sync::atomic::Ordering::Acquire)
}

fn declare_drive_range(app: &mut App, target: Entity, base: f32) {
    app.world_mut()
        .entity_mut(target)
        .insert(ModParamRange::default().with(ParamAddr::Unit(UnitParam::Drive), base, 0.0, 10.0));
}

/// Advance the transport by `samples`, as the audio clock would.
fn advance_transport(app: &mut App, samples: i64) {
    let transport = app.world().resource::<TransportRes>().clone();
    let current = transport.settings.steady_time();
    transport
        .settings
        .steady_time
        .store(current + samples, std::sync::atomic::Ordering::Relaxed);
}

#[test]
fn a_route_moves_the_target_nodes_own_atomic() {
    let (mut app, target) = app_with_graph();
    declare_drive_range(&mut app, target, 5.0);

    let lfo = app
        .world_mut()
        .spawn((
            ModSource::new(LfoShape::Sine),
            ModSourceRate::free_running(Hz(2.0)),
        ))
        .id();
    app.world_mut().spawn(
        ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth::FULL),
    );

    app.update();
    assert_eq!(
        app.world().resource::<ModulationMatrix>().len(),
        1,
        "the route should have resolved to one target"
    );

    // A sine starts at zero, so the first frame sits at base; run far enough
    // into the cycle for it to have swung.
    let mut moved = false;
    for _ in 0..30 {
        advance_transport(&mut app, 480); // 10ms at 48k
        app.update();
        if (node_drive(&app, target) - 5.0).abs() > 0.1 {
            moved = true;
        }
    }
    assert!(moved, "the LFO should have moved the node's drive atomic");
}

#[test]
fn an_unregistered_node_type_resolves_to_nothing() {
    // The registry is what makes resolution possible; without the node type
    // registered a route is inert rather than panicking.
    let mut app = App::new();
    let mut net = Net::new(0, 1);
    let node = net.push(Box::new(drive_node()));
    net.pipe_output(node);
    app.insert_resource(AudioGraphRes(net));
    app.insert_resource(TransportRes(Transport::new(48_000.0)));
    app.insert_resource(AudioEngineState::Running);
    app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));
    // Deliberately no `.register::<DistortionNode>()`.

    let target = app.world_mut().spawn(AudioNode(node)).id();
    declare_drive_range(&mut app, target, 5.0);
    let lfo = app
        .world_mut()
        .spawn((
            ModSource::new(LfoShape::Sine),
            ModSourceRate::free_running(Hz(2.0)),
        ))
        .id();
    app.world_mut().spawn(ModRoute::new(
        lfo,
        target,
        ParamAddr::Unit(UnitParam::Drive),
    ));

    app.update();

    assert!(app.world().resource::<ModulationMatrix>().is_empty());
    assert_eq!(
        node_drive(&app, target),
        UNMODULATED_DRIVE,
        "the node keeps its own value"
    );
}

#[test]
fn a_param_without_a_declared_range_is_not_modulated() {
    // `ModParamRange` is how a host says "this is modulatable, over this
    // range". Without it there is no base or clamp to accumulate against.
    let (mut app, target) = app_with_graph();
    let lfo = app
        .world_mut()
        .spawn((
            ModSource::new(LfoShape::Sine),
            ModSourceRate::free_running(Hz(2.0)),
        ))
        .id();
    app.world_mut().spawn(ModRoute::new(
        lfo,
        target,
        ParamAddr::Unit(UnitParam::Drive),
    ));

    app.update();

    assert!(app.world().resource::<ModulationMatrix>().is_empty());
}

#[test]
fn the_claim_set_reports_which_params_are_modulated() {
    let (mut app, target) = app_with_graph();
    declare_drive_range(&mut app, target, 5.0);
    let lfo = app
        .world_mut()
        .spawn((
            ModSource::new(LfoShape::Sine),
            ModSourceRate::free_running(Hz(2.0)),
        ))
        .id();
    app.world_mut().spawn(ModRoute::new(
        lfo,
        target,
        ParamAddr::Unit(UnitParam::Drive),
    ));

    app.update();

    let matrix = app.world().resource::<ModulationMatrix>();
    assert!(matrix.is_modulated(target, ParamAddr::Unit(UnitParam::Drive)));
    // A param nobody routed to is the reconciler's to write.
    assert!(!matrix.is_modulated(target, ParamAddr::Unit(UnitParam::Cutoff)));
}

// The two `set_base` tests moved into `modulation/driver.rs` when the method
// became `pub(crate)` — an integration test cannot reach it. They still build a
// real `App` and assert on the node's atomic; only their address changed. The
// public path they used to stand in for is covered by
// `an_authored_write_to_a_modulated_param_moves_the_base` in `audio_param.rs`.

#[test]
fn removing_a_route_returns_the_param_to_its_base() {
    // The continuous-value tax: modulation offsets are never "released", so a
    // deleted edge would leave its last offset stuck on the param forever if
    // the stale-layer sweep did not clear it.
    let (mut app, target) = app_with_graph();
    declare_drive_range(&mut app, target, 5.0);
    let lfo = app
        .world_mut()
        .spawn((
            ModSource::new(LfoShape::Square),
            ModSourceRate::free_running(Hz(0.0)),
        ))
        .id();
    let route = app
        .world_mut()
        .spawn(ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.2)))
        .id();

    app.update();
    advance_transport(&mut app, 480);
    app.update();
    let modulated = node_drive(&app, target);
    assert!(
        (modulated - 5.0).abs() > 0.1,
        "square at full depth should hold the param off its base"
    );

    app.world_mut().entity_mut(route).despawn();
    advance_transport(&mut app, 480);
    app.update();

    assert!(
        (node_drive(&app, target) - 5.0).abs() < 1e-3,
        "the removed route's layer must be cleared, not left stuck"
    );
    assert!(app.world().resource::<ModulationMatrix>().is_empty());
}

#[test]
fn two_routes_onto_one_param_sum_instead_of_overwriting() {
    // Distinct layer keys per route are what makes this hold: with a shared key
    // the second route would overwrite the first's contribution in place.
    let (mut app, target) = app_with_graph();
    declare_drive_range(&mut app, target, 5.0);

    let mut spawn_square = || {
        app.world_mut()
            .spawn((
                ModSource::new(LfoShape::Square),
                ModSourceRate::free_running(Hz(0.0)),
            ))
            .id()
    };
    let (a, b) = (spawn_square(), spawn_square());

    for source in [a, b] {
        app.world_mut().spawn(
            ModRoute::new(source, target, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.1)),
        );
    }

    app.update();
    advance_transport(&mut app, 480);
    app.update();
    let two = node_drive(&app, target);

    // One route alone, for comparison.
    let (mut app, target) = app_with_graph();
    declare_drive_range(&mut app, target, 5.0);
    let lfo = app
        .world_mut()
        .spawn((
            ModSource::new(LfoShape::Square),
            ModSourceRate::free_running(Hz(0.0)),
        ))
        .id();
    app.world_mut().spawn(
        ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.1)),
    );
    app.update();
    advance_transport(&mut app, 480);
    app.update();
    let one = node_drive(&app, target);

    assert!(
        (two - 5.0).abs() > (one - 5.0).abs() * 1.5,
        "two routes should push further than one: one={one}, two={two}"
    );
}

#[test]
fn a_disabled_route_contributes_nothing() {
    let (mut app, target) = app_with_graph();
    declare_drive_range(&mut app, target, 5.0);
    let lfo = app
        .world_mut()
        .spawn((
            ModSource::new(LfoShape::Square),
            ModSourceRate::free_running(Hz(0.0)),
        ))
        .id();
    let mut route =
        ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.2));
    route.enabled = false;
    app.world_mut().spawn(route);

    app.update();
    advance_transport(&mut app, 480);
    app.update();

    assert!(app.world().resource::<ModulationMatrix>().is_empty());
    assert_eq!(node_drive(&app, target), UNMODULATED_DRIVE);
}

#[test]
fn a_steady_transport_does_not_rebuild_the_matrix() {
    // Rebuilding mints fresh sources, and a fresh source starts at phase zero.
    // If a quiet frame rebuilt, every LFO would restart 60 times a second and
    // never advance — so the change-gate is load-bearing, not an optimization.
    let (mut app, target) = app_with_graph();
    declare_drive_range(&mut app, target, 5.0);
    let lfo = app
        .world_mut()
        .spawn((
            ModSource::new(LfoShape::Sine),
            ModSourceRate::free_running(Hz(2.0)),
        ))
        .id();
    app.world_mut().spawn(ModRoute::new(
        lfo,
        target,
        ParamAddr::Unit(UnitParam::Drive),
    ));

    app.update();
    let first = app
        .world()
        .resource::<ModulationMatrix>()
        .target(target, ParamAddr::Unit(UnitParam::Drive))
        .cloned()
        .expect("resolved");

    for _ in 0..5 {
        advance_transport(&mut app, 480);
        app.update();
    }

    let later = app
        .world()
        .resource::<ModulationMatrix>()
        .target(target, ParamAddr::Unit(UnitParam::Drive))
        .cloned()
        .expect("still resolved");

    assert!(
        std::sync::Arc::ptr_eq(&first, &later),
        "a quiet frame must not rebuild the matrix"
    );
}

/// Reflection has to reach the *leaves* to be worth anything: an editor showing
/// a route needs the `Depth` inside it, not just the struct's name. Walking down
/// to the float is what distinguishes real reflection from a derive that
/// compiles.
#[test]
fn a_route_reflects_down_to_its_depth() {
    use bevy_reflect::{PartialReflect, ReflectRef};

    // Real entities rather than synthesized ids: nothing here dereferences
    // them, but spawning keeps the test off `Entity`'s construction API.
    let mut world = World::new();
    let (a, b) = (world.spawn_empty().id(), world.spawn_empty().id());
    let route = ModRoute::new(a, b, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.25));

    let ReflectRef::Struct(s) = route.reflect_ref() else {
        panic!("ModRoute should reflect as a struct");
    };
    let depth = s.field("depth").expect("a `depth` field");

    // A unit newtype is a tuple struct, so it reflects with an indexed field —
    // and reflecting *through* to that float is the whole point: an opaque
    // value would stop the walk here.
    let ReflectRef::TupleStruct(depth) = depth.reflect_ref() else {
        panic!("Depth should reflect as a tuple struct, not an opaque value");
    };
    let inner = depth
        .field(0)
        .expect("Depth's inner float")
        .try_downcast_ref::<f32>()
        .expect("f32");
    assert_eq!(*inner, 0.25);
}

/// The types are registered, so a scene or an inspector can find them by name
/// rather than only through a value that already exists.
#[test]
fn the_components_are_registered_for_reflection() {
    use bevy_ecs::reflect::AppTypeRegistry;

    let (app, _) = app_with_graph();
    let registry = app.world().resource::<AppTypeRegistry>().read();

    for name in [
        std::any::type_name::<ModSource>(),
        std::any::type_name::<ModSourceRate>(),
        std::any::type_name::<ModRoute>(),
        std::any::type_name::<ModParamRange>(),
    ] {
        assert!(
            registry.get_with_type_path(name).is_some(),
            "{name} should be registered"
        );
    }
}
