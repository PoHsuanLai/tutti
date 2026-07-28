//! Unwiring an entity from the graph takes `AudioNode` off, not a second handle.
//!
//! `plugin_crash_detect_system` used to strip a crashed plugin of
//! `AudioEmitter` and remove its node from the graph by hand. That left
//! `AudioNode` in place, so the `On<Remove, AudioNode>` observers never fired:
//! the entity still claimed a node that was gone, and MIDI unregistration —
//! which keys on that same removal — never ran, leaking a sender on the bus for
//! the life of the process.
//!
//! # Why a synth rather than a crashed plugin
//!
//! `PluginEmitter` carries a `PluginHandle`, which is eight `Arc<dyn …>`
//! collaborators around a live plugin subprocess — not constructible headless,
//! and a crash is not producible on demand. But the defect was never about
//! plugins: it was that removing the wrong component unwires nothing. That is
//! what these assert, on a `PolySynth` that registers through the same path.

#![cfg(all(feature = "midi", feature = "synth"))]

use bevy_app::prelude::*;
use bevy_ecs::entity::Entity;

use bevy_tutti::graph::{AudioEmitter, AudioGraphRes, GraphReconcilePlugin};
use bevy_tutti::midi::{MidiBusRes, MidiTargetRegistry, TuttiMidiPlugin};
use bevy_tutti::AudioEngineState;
use tutti_core::dsp::Net;
use tutti_core::{AudioNode, NodeId};
use tutti_midi_types::MidiUnitId;
use tutti_synth::{PolySynth, SynthConfig};

/// An app whose graph, bus, and registry are wired the way `build_into` leaves
/// them, minus the audio device.
fn app() -> App {
    let mut app = App::new();
    let mut net = Net::new(0, 2);
    // Removing a node marks the graph dirty, and Commit-phase `commit_graph`
    // asserts a backend exists before publishing.
    let _backend = net.backend();
    app.insert_resource(AudioGraphRes(net));
    app.insert_resource(bevy_tutti::graph::TransportRes(
        tutti_core::transport::Transport::new(48_000.0),
    ));
    app.insert_resource(bevy_tutti::graph::AudioConfig {
        sample_rate: 48_000.0,
        channels: Default::default(),
    });
    app.insert_resource(AudioEngineState::Running);
    app.insert_resource(bevy_tutti::midi::test_support::midi_bus_for_test());
    app.insert_resource(bevy_tutti::midi::test_support::clock_master_for_test(48_000.0));
    app.add_plugins((GraphReconcilePlugin, TuttiMidiPlugin));
    app.world_mut()
        .resource_mut::<MidiTargetRegistry>()
        .register::<PolySynth>();
    app
}

/// A synth carrying both handles, as the crashed-plugin path would have it.
fn spawn_synth(app: &mut App) -> (Entity, NodeId, MidiUnitId) {
    let synth = PolySynth::new(SynthConfig::default()).expect("builds a synth");
    let unit_id = synth.midi_port().unit_id();
    let node = {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        graph.0.push(Box::new(synth))
    };
    let entity = app
        .world_mut()
        .spawn((AudioNode(node), AudioEmitter { node_id: node }))
        .id();
    (entity, node, unit_id)
}

fn bus_has(app: &App, unit_id: MidiUnitId) -> bool {
    app.world().resource::<MidiBusRes>().contains(unit_id)
}

fn graph_has(app: &App, node: NodeId) -> bool {
    app.world().resource::<AudioGraphRes>().0.contains(node)
}

/// Removing `AudioNode` unwires both the graph node and the MIDI sender, with
/// no help from the caller.
///
/// This is what the crash path now does, and why it no longer touches the graph
/// itself: one component removal drives both observers.
#[test]
fn removing_the_node_handle_unwires_the_graph_and_the_bus() {
    let mut app = app();
    let (entity, node, unit_id) = spawn_synth(&mut app);
    app.update();
    assert!(bus_has(&app, unit_id), "registered to begin with");
    assert!(graph_has(&app, node), "and in the graph");

    app.world_mut().entity_mut(entity).remove::<AudioNode>();
    app.update();

    assert!(
        !graph_has(&app, node),
        "the observer removes the node — the crash system must not do it by hand"
    );
    assert!(
        !bus_has(&app, unit_id),
        "and MIDI unregistration rides on the same removal"
    );
}

/// The bug, pinned: removing only the second handle unwires nothing.
///
/// This is exactly what `plugin_crash_detect_system` did. Both assertions here
/// describe the leak, so if anyone reintroduces an `AudioEmitter`-only removal
/// this test keeps passing while the one above starts failing — which is the
/// pair working as intended.
#[test]
fn removing_only_the_emitter_leaves_both_behind() {
    let mut app = app();
    let (entity, node, unit_id) = spawn_synth(&mut app);
    app.update();

    app.world_mut().entity_mut(entity).remove::<AudioEmitter>();
    app.update();

    assert!(
        graph_has(&app, node),
        "no observer keys on AudioEmitter, so the node survives — the leak"
    );
    assert!(
        bus_has(&app, unit_id),
        "and the sender stays on the bus forever"
    );
}
