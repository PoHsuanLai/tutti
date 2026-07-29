//! A modulator kind the adapter has never heard of, driving a real param.
//!
//! The registry's whole claim is that `tutti-mod`'s genericity survives into
//! the ECS layer: `Modulator` is generic over its state, `Sourced<M>` erases
//! `M`, so an app should be able to add a kind without bevy-tutti knowing it.
//! Registering only types the adapter already ships would not test that — this
//! defines a modulator here, in the test, and drives a node with it.

#![cfg(feature = "modulation")]

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;

use bevy_tutti::graph::{AudioGraphRes, GraphReconcilePlugin, TransportRes};
use bevy_tutti::modulation::{
    ModParamRange, ModRate, ModRoute, ModSource, ModSourceAppExt, ModSourceKind, ModTargetRegistry,
    ModulationMatrix, TuttiModulationPlugin,
};
use bevy_tutti::AudioEngineState;
use tutti_core::dsp::Net;
use tutti_core::transport::Transport;
use tutti_core::AudioNode;
use tutti_mod::Modulator;
use tutti_types::{Depth, Hz, ParamAddr, Phase, UnitParam};
use tutti_units::DistortionNode;

/// A modulator with no analogue in `tutti-mod`: a two-step stair, held for
/// half a cycle each. Stateless, so its `State` is `()` — the simplest thing
/// the trait allows, and enough to prove the erasure works.
struct Stair;

impl Modulator for Stair {
    type State = ();

    fn value(&self, _state: (), phase: Phase) -> ((), f32) {
        ((), if phase.get() < 0.5 { -1.0 } else { 1.0 })
    }
}

/// The ECS declaration of a `Stair`, carrying a parameter `tutti-mod` has no
/// concept of — proof that a kind owns its own config rather than squeezing
/// into a shared `ModSource`.
#[derive(Component, Clone)]
struct StairSource {
    /// Scales the stair's two levels. Nothing in bevy-tutti knows this exists.
    amount: f32,
}

impl ModSourceKind for StairSource {
    type Source = ScaledStair;

    fn build(&self) -> ScaledStair {
        ScaledStair {
            amount: self.amount,
        }
    }
}

struct ScaledStair {
    amount: f32,
}

impl Modulator for ScaledStair {
    type State = ();

    fn value(&self, _state: (), phase: Phase) -> ((), f32) {
        let (_, v) = Stair.value((), phase);
        ((), v * self.amount)
    }
}

const BASE_DRIVE: f32 = 5.0;

fn app_with_node() -> (App, Entity) {
    let mut app = App::new();

    let mut net = Net::new(0, 1);
    let node = net.push(Box::new(DistortionNode::new(
        tutti_units::ShapeKind::Tanh,
        1.0,
    )));
    net.pipe_output(node);

    app.insert_resource(AudioGraphRes(net));
    app.insert_resource(TransportRes(Transport::new(48_000.0)));
    app.insert_resource(AudioEngineState::Running);
    app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));
    app.world_mut()
        .resource_mut::<ModTargetRegistry>()
        .register::<DistortionNode>();

    let target = app.world_mut().spawn(AudioNode(node)).id();
    app.world_mut()
        .entity_mut(target)
        .insert(ModParamRange::default().with(
            ParamAddr::Unit(UnitParam::Drive),
            BASE_DRIVE,
            0.0,
            10.0,
        ));
    (app, target)
}

fn node_drive(app: &App, entity: Entity) -> f32 {
    let node = app.world().get::<AudioNode>(entity).unwrap().0;
    app.world()
        .resource::<AudioGraphRes>()
        .0
        .node_as::<DistortionNode>(node)
        .unwrap()
        .drive()
        .load(std::sync::atomic::Ordering::Acquire)
}

fn advance_transport(app: &mut App, samples: i64) {
    let transport = app.world().resource::<TransportRes>().clone();
    let current = transport.settings.steady_time();
    transport
        .settings
        .steady_time
        .store(current + samples, std::sync::atomic::Ordering::Relaxed);
}

#[test]
fn a_custom_kind_drives_a_param() {
    let (mut app, target) = app_with_node();
    app.add_mod_source::<StairSource>();

    let source = app
        .world_mut()
        .spawn((StairSource { amount: 1.0 }, ModRate::free_running(Hz(10.0))))
        .id();
    app.world_mut().spawn(
        ModRoute::new(source, target, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.2)),
    );

    app.update();
    assert_eq!(
        app.world().resource::<ModulationMatrix>().len(),
        1,
        "the custom kind should have resolved a target"
    );

    // The stair steps at half a cycle; sweep a full one and require the drive
    // to visit two distinct levels.
    let mut seen: Vec<f32> = Vec::new();
    for _ in 0..20 {
        advance_transport(&mut app, 480);
        app.update();
        let v = node_drive(&app, target);
        if !seen.iter().any(|s| (s - v).abs() < 1e-3) {
            seen.push(v);
        }
    }

    assert!(
        seen.len() >= 2,
        "a stair should have driven the param to two levels, saw {seen:?}"
    );
    assert!(
        seen.iter().any(|v| *v > BASE_DRIVE),
        "one level should sit above base: {seen:?}"
    );
    assert!(seen.iter().any(|v| *v < BASE_DRIVE), "one below: {seen:?}");
}

/// The kind's own parameters must reach the built modulator — otherwise the
/// registry is just a type-level ceremony over a fixed source.
#[test]
fn the_kinds_own_config_reaches_the_modulator() {
    let mut depths = Vec::new();
    for amount in [0.25_f32, 1.0] {
        let (mut app, target) = app_with_node();
        app.add_mod_source::<StairSource>();

        let source = app
            .world_mut()
            .spawn((StairSource { amount }, ModRate::free_running(Hz(10.0))))
            .id();
        app.world_mut().spawn(
            ModRoute::new(source, target, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.2)),
        );

        let mut extreme: f32 = 0.0;
        for _ in 0..20 {
            advance_transport(&mut app, 480);
            app.update();
            extreme = extreme.max((node_drive(&app, target) - BASE_DRIVE).abs());
        }
        depths.push(extreme);
    }

    assert!(
        depths[1] > depths[0] * 2.0,
        "amount 1.0 should swing far wider than 0.25: {depths:?}"
    );
}

/// The built-in kind and a custom one coexist: two registered kinds means two
/// collectors, and both must land in the one registry the routes index into.
///
/// They drive the *same* param, since a distortion node exposes only `Drive`.
/// That also exercises the summing path — two sources, two layers, one
/// accumulator — across a kind boundary.
#[test]
fn a_built_in_and_a_custom_kind_coexist() {
    let (mut app, target) = app_with_node();
    app.add_mod_source::<StairSource>();

    let lfo = app
        .world_mut()
        .spawn((
            ModSource::new(bevy_tutti::modulation::LfoShape::Sine),
            ModRate::free_running(Hz(10.0)),
        ))
        .id();
    let stair = app
        .world_mut()
        .spawn((StairSource { amount: 1.0 }, ModRate::free_running(Hz(10.0))))
        .id();

    app.world_mut().spawn(
        ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.1)),
    );
    app.world_mut().spawn(
        ModRoute::new(stair, target, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.1)),
    );

    app.update();
    assert!(app
        .world()
        .resource::<ModulationMatrix>()
        .is_modulated(target, ParamAddr::Unit(UnitParam::Drive)));

    // Both contribute: the stair alone would step between two levels, so a
    // third distinct value can only come from the sine summing with it.
    let mut seen: Vec<f32> = Vec::new();
    for _ in 0..40 {
        advance_transport(&mut app, 480);
        app.update();
        let v = node_drive(&app, target);
        if !seen.iter().any(|s| (s - v).abs() < 1e-3) {
            seen.push(v);
        }
    }
    assert!(
        seen.len() > 2,
        "two summed sources should visit more than the stair's own two levels, saw {seen:?}"
    );
}

/// Registering a kind twice must schedule one collector. Two would each push a
/// source for the same entity; the second would take a registry index no route
/// points at, and its modulation would silently never apply.
#[test]
fn registering_a_kind_twice_is_idempotent() {
    let (mut app, target) = app_with_node();
    app.add_mod_source::<StairSource>()
        .add_mod_source::<StairSource>();

    let source = app
        .world_mut()
        .spawn((StairSource { amount: 1.0 }, ModRate::free_running(Hz(10.0))))
        .id();
    app.world_mut().spawn(
        ModRoute::new(source, target, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.2)),
    );

    // `rebuild` drains the collected sources, so inspect them before it runs:
    // a doubly-registered kind builds two sources for the one entity, and the
    // second takes a registry index no route points at.
    app.world_mut().run_schedule(bevy_app::Update);
    let collected = app
        .world()
        .resource::<bevy_tutti::modulation::CollectedModSources>();
    assert!(
        collected.len() <= 1,
        "one registration's worth of sources, not {}",
        collected.len()
    );

    // And the route still resolves, which a duplicate index would break.
    app.update();
    assert!(app
        .world()
        .resource::<ModulationMatrix>()
        .is_modulated(target, ParamAddr::Unit(UnitParam::Drive)));
}
