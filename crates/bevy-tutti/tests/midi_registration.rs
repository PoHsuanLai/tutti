//! A MIDI node's sender reaches the bus, and leaves it again.
//!
//! The half that was missing is the leaving: the previous layer registered a
//! sender inline in the soundfont spawner and never removed one anywhere, so the
//! bus grew by an entry per spawn and `MidiUnitId`s — from a monotonic counter,
//! never reused — accumulated for the life of the process.
//!
//! The synth here is a `PolySynth` rather than a `SoundFontUnit` because the
//! latter needs a `.sf2` on disk; both own a `MidiInPort` and register the same
//! way, which is the whole point of resolving through a registry.

#![cfg(all(feature = "midi", feature = "synth"))]

use bevy_app::prelude::*;

use bevy_tutti::graph::{AudioGraphRes, GraphReconcilePlugin};
use bevy_tutti::midi::{MidiRegistered, MidiTargetRegistry, TuttiMidiPlugin};
use bevy_tutti::AudioEngineState;
use tutti_core::dsp::Net;
use tutti_core::AudioNode;
use tutti_synth::{PolySynth, SynthConfig};

/// An app with a graph, a MIDI bus, and `PolySynth` registered as addressable.
///
/// `MidiBusRes` cannot be `init_resource`d — it must be the instance the RT
/// pre-block shares — so this uses the crate's own test constructor to stand one
/// up without booting an audio device.
fn app() -> App {
    let mut app = bare_app();
    app.world_mut()
        .resource_mut::<MidiTargetRegistry>()
        .register::<PolySynth>();
    app
}

/// The same app without any node type registered.
///
/// The graph takes a `backend()`: removing a node marks it dirty, and the
/// Commit-phase `commit_graph` asserts a backend exists before publishing. A
/// backend-less graph is fine only for tests that never edit the topology.
fn bare_app() -> App {
    let mut app = App::new();
    let mut net = Net::new(0, 2);
    let _backend = net.backend();
    app.insert_resource(AudioGraphRes(net));
    // `AudioEngineState::Running` is a claim about the whole engine block, and
    // systems gated on `engine_ready` take everything that block inserts as
    // plain `Res` — so a test asserting readiness has to supply them all.
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
    // `engine_ready` claims every resource the engine block inserts is
    // present, and the route rebuild takes `MidiRoutingRes` as a plain
    // `ResMut` on that promise. A test asserting readiness supplies it.
    app.insert_resource(bevy_tutti::midi::test_support::routing_table_for_test().0);
    app.add_plugins((GraphReconcilePlugin, TuttiMidiPlugin));
    app
}

/// Add a synth to the graph and give an entity its `AudioNode`.
fn spawn_synth(app: &mut App) -> (bevy_ecs::entity::Entity, tutti_midi_types::MidiUnitId) {
    let synth = PolySynth::new(SynthConfig::default()).expect("builds a synth");
    let unit_id = synth.midi_port().unit_id();
    let node = {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        graph.0.push(Box::new(synth))
    };
    let entity = app.world_mut().spawn(AudioNode(node)).id();
    (entity, unit_id)
}

fn bus_has(app: &App, unit_id: tutti_midi_types::MidiUnitId) -> bool {
    app.world()
        .resource::<bevy_tutti::midi::MidiBusRes>()
        .contains(unit_id)
}

/// The bus learns a node's sender without anyone registering it by hand.
#[test]
fn a_midi_node_reaches_the_bus() {
    let mut app = app();
    let (entity, unit_id) = spawn_synth(&mut app);
    assert!(!bus_has(&app, unit_id), "not registered before a frame runs");

    app.update();

    assert!(bus_has(&app, unit_id), "the synth's sender should be routable");
    assert!(
        app.world().get::<MidiRegistered>(entity).is_some(),
        "and the entity should be marked as registered"
    );
}

/// The half that never existed: despawning takes the sender back off.
#[test]
fn a_despawned_node_leaves_the_bus() {
    let mut app = app();
    let (entity, unit_id) = spawn_synth(&mut app);
    app.update();
    assert!(bus_has(&app, unit_id));

    app.world_mut().despawn(entity);
    app.update();

    assert!(
        !bus_has(&app, unit_id),
        "a despawned synth must not keep routing MIDI — this is the leak"
    );
}

/// Removing just the node, keeping the entity, also unregisters — and the
/// entity can register again if a node comes back.
#[test]
fn removing_the_node_unregisters_and_re_registering_works() {
    let mut app = app();
    let (entity, first_id) = spawn_synth(&mut app);
    app.update();
    assert!(bus_has(&app, first_id));

    app.world_mut().entity_mut(entity).remove::<AudioNode>();
    app.update();
    assert!(!bus_has(&app, first_id), "removing the node unregisters it");
    assert!(
        app.world().get::<MidiRegistered>(entity).is_none(),
        "the marker is cleared, so the entity is registrable again"
    );

    // A fresh unit on the same entity registers under its own new id.
    let synth = PolySynth::new(SynthConfig::default()).expect("builds a synth");
    let second_id = synth.midi_port().unit_id();
    let node = {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        graph.0.push(Box::new(synth))
    };
    app.world_mut().entity_mut(entity).insert(AudioNode(node));
    app.update();

    assert_ne!(first_id, second_id, "a new port mints a new id");
    assert!(bus_has(&app, second_id), "the replacement registers");
}

/// An entity whose node type was never registered is skipped, not panicked on,
/// and does not block the ones that were.
#[test]
fn an_unregistered_node_type_is_skipped() {
    // Deliberately no `.register::<PolySynth>()`.
    let mut app = bare_app();

    let (entity, unit_id) = spawn_synth(&mut app);
    app.update();

    assert!(!bus_has(&app, unit_id), "nothing can resolve it");
    assert!(
        app.world().get::<MidiRegistered>(entity).is_none(),
        "and it stays unmarked, so it retries if the type is registered later"
    );
}
