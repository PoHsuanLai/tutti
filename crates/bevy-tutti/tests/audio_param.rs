//! `AudioParam` reconciled into a real graph, asserted on the node's own value.
//!
//! The interesting cases are the seams: a param reaching the node at all, a
//! steady frame doing nothing, and — the reason the claim set exists — an
//! authored write on a *modulated* param going to the accumulator base instead
//! of the atomic.

// The plain-reconcile tests below run in every configuration; the ones that
// need a modulation driver are gated individually. Gating the whole file would
// leave the `not(modulation)` branch of the reconciler compiled but never run.
use bevy_app::prelude::*;
use bevy_ecs::prelude::*;

use bevy_tutti::graph::{
    AudioGraphRes, AudioParam, AudioParamAppExt, GraphReconcilePlugin, TransportRes,
};
#[cfg(feature = "modulation")]
use bevy_tutti::modulation::{
    LfoShape, ModParamRange, ModRoute, ModSource, ModSourceRate, ModTargetRegistry,
    TuttiModulationPlugin,
};
use bevy_tutti::AudioEngineState;
use tutti_core::dsp::Net;
use tutti_core::transport::Transport;
use tutti_core::AudioNode;
use tutti_types::{Drive, Hz, UnitParam};
// Only the modulation tests below use these.
use tutti_nodes::DistortionNode;
#[cfg(feature = "modulation")]
use tutti_types::{Depth, ParamAddr};

/// The drive a freshly built node carries.
const INITIAL_DRIVE: f32 = 1.0;

/// `Drive` on a distortion node — a param with a readable atomic behind it.
type DriveParam = AudioParam<Drive, { UnitParam::Drive as u16 }>;

fn app_with_node() -> (App, Entity) {
    use tutti_core::dsp::AudioUnit as _;

    let mut app = App::new();

    let mut net = Net::new(0, 1);
    let node = net.push(Box::new(DistortionNode::new(
        tutti_nodes::ShapeKind::Tanh,
        INITIAL_DRIVE,
    )));
    net.pipe_output(node);
    net.set_sample_rate(tutti_core::SampleRate(48_000.0));
    // Deliberately no `backend()`. With one, `Net::set` enqueues to the audio
    // thread and the frontend vertex these tests read is never updated — every
    // assertion would compare against a stale value and the ones expecting "no
    // change" would pass for the wrong reason. Backend-less, `set` applies
    // straight to the vertex, which is the same code path the audio thread runs
    // on the other side of the queue.

    app.insert_resource(AudioGraphRes(net));
    app.insert_resource(TransportRes(Transport::new(48_000.0)));
    app.insert_resource(AudioEngineState::Running);
    app.add_plugins(GraphReconcilePlugin);
    #[cfg(feature = "modulation")]
    app.add_plugins(TuttiModulationPlugin);
    app.add_audio_param::<Drive, { UnitParam::Drive as u16 }>();

    let entity = app.world_mut().spawn(AudioNode(node)).id();
    (app, entity)
}

/// The node's live drive — what the DSP reads.
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

#[test]
fn an_inserted_param_reaches_the_node() {
    let (mut app, entity) = app_with_node();

    app.world_mut()
        .entity_mut(entity)
        .insert(DriveParam::new(Drive(4.0)));
    app.update();

    assert_eq!(node_drive(&app, entity), 4.0);
}

#[test]
fn a_changed_param_reaches_the_node() {
    let (mut app, entity) = app_with_node();
    app.world_mut()
        .entity_mut(entity)
        .insert(DriveParam::new(Drive(4.0)));
    app.update();

    app.world_mut()
        .entity_mut(entity)
        .insert(DriveParam::new(Drive(7.5)));
    app.update();

    assert_eq!(node_drive(&app, entity), 7.5);
}

/// Change detection is the whole gate: without it every param would push every
/// frame, and `Net::set` would enqueue a message per param per frame forever.
#[test]
fn an_unchanged_param_does_not_push() {
    let (mut app, entity) = app_with_node();
    app.world_mut()
        .entity_mut(entity)
        .insert(DriveParam::new(Drive(4.0)));
    app.update();

    // Move the node's value behind the reconciler's back. A push would restore
    // it to 4.0; silence leaves the poke standing.
    let node = app.world().get::<AudioNode>(entity).unwrap().0;
    {
        let graph = app.world().resource::<AudioGraphRes>();
        graph
            .0
            .node_as::<DistortionNode>(node)
            .unwrap()
            .set_drive(Drive(9.0));
    }

    app.update();

    assert_eq!(
        node_drive(&app, entity),
        9.0,
        "an unchanged param must not re-push"
    );
}

/// The param's address is what distinguishes two params of the same unit, so a
/// component addressing a param the node does not expose must be inert rather
/// than landing on some other param.
#[test]
fn a_param_the_node_does_not_expose_is_inert() {
    let (mut app, entity) = app_with_node();
    app.add_audio_param::<Hz, { UnitParam::Cutoff as u16 }>();

    app.world_mut()
        .entity_mut(entity)
        .insert(AudioParam::<Hz, { UnitParam::Cutoff as u16 }>::new(Hz(
            800.0,
        )));
    app.update();

    assert_eq!(
        node_drive(&app, entity),
        INITIAL_DRIVE,
        "a distortion node has no cutoff; drive must be untouched"
    );
}

/// Registering the same param twice must schedule one system. Two would each
/// push the same value — harmless to the result, but it doubles the per-frame
/// cost and makes the schedule depend on how many callers happened to ask.
#[test]
fn registering_a_param_twice_is_idempotent() {
    let (mut app, entity) = app_with_node();
    app.add_audio_param::<Drive, { UnitParam::Drive as u16 }>()
        .add_audio_param::<Drive, { UnitParam::Drive as u16 }>();

    app.world_mut()
        .entity_mut(entity)
        .insert(DriveParam::new(Drive(4.0)));
    app.update();

    assert_eq!(node_drive(&app, entity), 4.0);
}

/// The single-writer rule, which is the reason the claim set exists.
///
/// Modulation flushes `base + Σ offsets` into the node atomic every frame. A
/// plain param write to the same atomic would be reverted within a frame — the
/// fader would move on screen and not in the sound. The reconciler must instead
/// move the accumulator's *base*, so the authored value rides under the
/// modulation.
#[cfg(feature = "modulation")]
#[test]
fn an_authored_write_to_a_modulated_param_moves_the_base() {
    let (mut app, entity) = app_with_node();
    app.world_mut()
        .resource_mut::<ModTargetRegistry>()
        .register::<DistortionNode>();

    app.world_mut().entity_mut(entity).insert((
        DriveParam::new(Drive(5.0)),
        ModParamRange::default().with(ParamAddr::Unit(UnitParam::Drive), 5.0, 0.0, 10.0),
    ));
    // A square at zero rate holds a constant offset, so the base shift stays
    // legible against it.
    let lfo = app
        .world_mut()
        .spawn((
            ModSource::new(LfoShape::Square),
            ModSourceRate::free_running(Hz(0.0)),
        ))
        .id();
    app.world_mut().spawn(
        ModRoute::new(lfo, entity, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.1)),
    );

    app.update();
    let modulated = node_drive(&app, entity);

    // Author a new value while modulation owns the param.
    app.world_mut()
        .entity_mut(entity)
        .insert(DriveParam::new(Drive(8.0)));
    app.update();
    let after = node_drive(&app, entity);

    assert!(
        (after - modulated - 3.0).abs() < 0.2,
        "base 5 -> 8 should carry through the modulation: {modulated} -> {after}"
    );
}
