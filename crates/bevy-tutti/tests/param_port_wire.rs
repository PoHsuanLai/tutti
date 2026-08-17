//! Spike: declaring an audio-rate **param** port through `PortSources`.
//!
//! The imperative form of this edge is fragile — `Net::pipe_input` walks every
//! input port of a node, so a later "wire the audio in" call silently
//! overwrites a param edge (see tutti-units' `audio_rate_param_mod` test). This
//! file asks whether routing the same edge through the declarative layer makes
//! that clobber unrepresentable.

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;

use bevy_tutti::graph::{
    AudioGraphRes, GraphReconcilePlugin, MasterSources, PortSource, PortSources,
};
use bevy_tutti::AudioEngineState;
use tutti_core::dsp::{sine_hz, Net, Source};
use tutti_core::AudioNode;
use tutti_types::UnitParam;
use tutti_units::{AtomicSourceUnit, DistortionNode, ParamPorts, ParamSumUnit, ShapeKind};

fn app() -> App {
    let mut app = App::new();
    app.insert_resource(AudioGraphRes(Net::with_backend(2)));
    app.insert_resource(AudioEngineState::Running);
    app.add_plugins(GraphReconcilePlugin);
    app
}

fn spawn_node<U: tutti_core::dsp::AudioUnit + 'static>(app: &mut App, unit: U) -> Entity {
    let id = {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        graph.0.add(unit)
    };
    app.world_mut().spawn(AudioNode(id)).id()
}

fn node_id(app: &App, entity: Entity) -> tutti_core::NodeId {
    app.world().get::<AudioNode>(entity).expect("AudioNode").0
}

/// The headline: audio and param ports declared on **one** component, both
/// reaching the engine.
///
/// This is what makes the port space have a single writer — the thing the
/// imperative form cannot guarantee.
#[test]
fn audio_and_param_ports_are_declared_together() {
    let mut app = app();

    // A distortion born with its drive port on: inputs are [L, R, drive].
    let dist = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 5.0, true);
    let drive_port = dist.param_port(UnitParam::Drive).expect("drive port");
    assert_eq!(drive_port, 2, "the param port follows the audio inputs");

    let target = spawn_node(&mut app, dist);
    let osc = spawn_node(&mut app, sine_hz::<f32>(440.0));
    // The base-sum chain feeding the param port.
    let base = spawn_node(&mut app, AtomicSourceUnit::new(9.0));
    let sum = spawn_node(&mut app, ParamSumUnit::new(0, 0.0, 10.0));

    // ONE declaration covering both kinds of port.
    app.world_mut()
        .entity_mut(sum)
        .insert(PortSources::from(base));
    app.world_mut().entity_mut(target).insert(
        PortSources::silent()
            .with(
                0,
                PortSource::Node {
                    entity: osc,
                    port: 0,
                },
            )
            .with(
                1,
                PortSource::Node {
                    entity: osc,
                    port: 0,
                },
            )
            .with(
                drive_port,
                PortSource::Node {
                    entity: sum,
                    port: 0,
                },
            ),
    );
    app.update();

    let (target_id, osc_id, sum_id, base_id) = (
        node_id(&app, target),
        node_id(&app, osc),
        node_id(&app, sum),
        node_id(&app, base),
    );
    let graph = app.world().resource::<AudioGraphRes>();

    assert_eq!(graph.0.source(target_id, 0), Source::Local(osc_id, 0));
    assert_eq!(graph.0.source(target_id, 1), Source::Local(osc_id, 0));
    assert_eq!(
        graph.0.source(target_id, drive_port),
        Source::Local(sum_id, 0),
        "the param port is fed by the sum, declared alongside the audio"
    );
    assert_eq!(graph.0.source(sum_id, 0), Source::Local(base_id, 0));
}

/// The clobber the imperative form suffers cannot be expressed here.
///
/// `PortSources` is one component per entity (the ECS enforces that), and
/// `rebuild` writes the whole declared port range from that one `Vec`. So
/// "something else overwrote the param port" has no representation: re-declaring
/// the audio ports means editing the same `Vec` that holds the param port, and
/// a `Vec` shorter than the param index leaves it *undeclared* — untouched, not
/// zeroed.
#[test]
fn redeclaring_audio_does_not_disturb_the_param_port() {
    let mut app = app();

    let dist = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 5.0, true);
    let drive_port = dist.param_port(UnitParam::Drive).unwrap();
    let target = spawn_node(&mut app, dist);
    let osc = spawn_node(&mut app, sine_hz::<f32>(440.0));
    let other = spawn_node(&mut app, sine_hz::<f32>(220.0));
    let sum = spawn_node(&mut app, ParamSumUnit::new(0, 0.0, 10.0));

    app.world_mut().entity_mut(target).insert(
        PortSources::silent()
            .with(
                0,
                PortSource::Node {
                    entity: osc,
                    port: 0,
                },
            )
            .with(
                drive_port,
                PortSource::Node {
                    entity: sum,
                    port: 0,
                },
            ),
    );
    app.update();

    let (target_id, sum_id) = (node_id(&app, target), node_id(&app, sum));
    assert_eq!(
        app.world()
            .resource::<AudioGraphRes>()
            .0
            .source(target_id, drive_port),
        Source::Local(sum_id, 0)
    );

    // Now re-point the AUDIO input — the operation that, imperatively, would
    // have been `pipe_input` and would have taken the param edge with it.
    let mut decl = app.world_mut().get_mut::<PortSources>(target).unwrap();
    decl.0[0] = PortSource::Node {
        entity: other,
        port: 0,
    };
    app.update();

    let other_id = node_id(&app, other);
    let graph = app.world().resource::<AudioGraphRes>();
    assert_eq!(
        graph.0.source(target_id, 0),
        Source::Local(other_id, 0),
        "the audio input moved"
    );
    assert_eq!(
        graph.0.source(target_id, drive_port),
        Source::Local(sum_id, 0),
        "and the param edge is untouched — one writer owns the whole port space"
    );
}

/// A param port the declaration does not mention is left alone, exactly as an
/// unmentioned audio port is. "Undeclared" and "declared silent" stay distinct.
#[test]
fn an_undeclared_param_port_is_untouched() {
    let mut app = app();

    let dist = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 5.0, true);
    let drive_port = dist.param_port(UnitParam::Drive).unwrap();
    let target = spawn_node(&mut app, dist);
    let osc = spawn_node(&mut app, sine_hz::<f32>(440.0));
    let sum = spawn_node(&mut app, ParamSumUnit::new(0, 0.0, 10.0));

    // Wire the param port imperatively first — a host that has not adopted the
    // declaration for it yet.
    let (target_id, sum_id) = (node_id(&app, target), node_id(&app, sum));
    {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        graph
            .0
            .set_source(target_id, drive_port, Source::Local(sum_id, 0));
    }

    // Declare ONLY the audio ports. The Vec stops before the param index.
    app.world_mut()
        .entity_mut(target)
        .insert(PortSources::from(osc));
    app.update();

    let graph = app.world().resource::<AudioGraphRes>();
    assert_eq!(
        graph.0.source(target_id, drive_port),
        Source::Local(sum_id, 0),
        "a short declaration leaves trailing ports undeclared, not silenced"
    );
}

/// The full chain, declared: global input → node audio, and
/// `base → sum → node.drive_port` for the modulation.
///
/// Structure only, matching this crate's other wiring tests — that the chain
/// *renders* (drive 9.0 saturating where 1.0 does not) is asserted in
/// tutti-units' `audio_rate_param_mod`, where a backend-free `Net` can be ticked
/// directly. A net with a backend defers to `commit`, so ticking the frontend
/// here would prove nothing about what the engine runs.
#[test]
fn the_whole_declared_chain_reaches_the_graph() {
    let mut app = app();
    app.insert_resource(MasterSources::default());

    let dist = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 1.0, true);
    let drive_port = dist.param_port(UnitParam::Drive).unwrap();
    let target = spawn_node(&mut app, dist);
    let osc = spawn_node(&mut app, sine_hz::<f32>(440.0));
    let base = spawn_node(&mut app, AtomicSourceUnit::new(9.0));
    let sum = spawn_node(&mut app, ParamSumUnit::new(0, 0.0, 10.0));

    app.world_mut()
        .entity_mut(sum)
        .insert(PortSources::from(base));
    app.world_mut().entity_mut(target).insert(
        PortSources::silent()
            .with(
                0,
                PortSource::Node {
                    entity: osc,
                    port: 0,
                },
            )
            .with(
                1,
                PortSource::Node {
                    entity: osc,
                    port: 0,
                },
            )
            .with(
                drive_port,
                PortSource::Node {
                    entity: sum,
                    port: 0,
                },
            ),
    );
    app.update();

    let (target_id, osc_id, sum_id, base_id) = (
        node_id(&app, target),
        node_id(&app, osc),
        node_id(&app, sum),
        node_id(&app, base),
    );
    let graph = app.world().resource::<AudioGraphRes>();

    // Audio in from the oscillator...
    assert_eq!(graph.0.source(target_id, 0), Source::Local(osc_id, 0));
    assert_eq!(graph.0.source(target_id, 1), Source::Local(osc_id, 0));
    // ...and the modulation chain into the param port, all from one declaration.
    assert_eq!(
        graph.0.source(target_id, drive_port),
        Source::Local(sum_id, 0)
    );
    assert_eq!(graph.0.source(sum_id, 0), Source::Local(base_id, 0));
}
