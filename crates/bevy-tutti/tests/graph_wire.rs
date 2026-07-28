//! Wiring declared in the ECS reaches the graph, and stops reaching it when the
//! declaration goes away.
//!
//! Every assertion reads the engine back through `Net::source` /
//! `Net::output_source` rather than trusting the component, because the diff
//! this layer performs is only meaningful if the engine is the thing being
//! compared against.

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;

use bevy_tutti::graph::{
    AudioGraphRes, AudioSource, AudioSources, GraphDirty, GraphReconcilePlugin,
    GraphReconcileSystems, MasterSources,
};
use bevy_tutti::AudioEngineState;
use tutti_core::dsp::{pass, sine_hz, Net, Source};
use tutti_core::AudioNode;

/// An app wired the way `build_into` leaves one, minus the audio device.
fn app() -> App {
    let mut app = App::new();
    app.insert_resource(AudioGraphRes(Net::with_backend(2)));
    app.insert_resource(AudioEngineState::Running);
    app.add_plugins(GraphReconcilePlugin);
    app
}

/// Add a node to the graph and bind an entity to it.
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

/// The headline claim: a declaration on the sink reaches the engine.
#[test]
fn a_declared_source_reaches_the_graph() {
    let mut app = app();
    let osc = spawn_node(&mut app, sine_hz::<f32>(440.0));
    let filt = spawn_node(&mut app, pass());

    app.world_mut()
        .entity_mut(filt)
        .insert(AudioSources::from(osc));
    app.update();

    let (osc_id, filt_id) = (node_id(&app, osc), node_id(&app, filt));
    assert_eq!(
        app.world().resource::<AudioGraphRes>().0.source(filt_id, 0),
        Source::Local(osc_id, 0)
    );
}

/// The master bus is one declaration with one value per channel, so two nodes
/// cannot both claim it.
///
/// This is the inverse of `master_bus.rs`'s
/// `a_second_pipe_output_silently_replaces_the_first`: there, the second caller
/// silently won. Here there is no second caller to have — a resource holds one
/// value, and a channel holds one source.
#[test]
fn the_master_bus_has_one_declaration_not_a_race() {
    let mut app = app();
    let a = spawn_node(&mut app, sine_hz::<f32>(440.0));
    let b = spawn_node(&mut app, sine_hz::<f32>(880.0));

    // Both nodes exist and both want the master. Only a declaration decides.
    app.world_mut()
        .insert_resource(MasterSources::from(a).with(1, AudioSource::node(b)));
    app.update();

    let graph = app.world().resource::<AudioGraphRes>();
    assert_eq!(graph.0.output_source(0), Source::Local(node_id(&app, a), 0));
    assert_eq!(graph.0.output_source(1), Source::Local(node_id(&app, b), 0));
}

/// Removing the declaration silences the ports it claimed.
///
/// The case that is unsolvable imperatively without every call site remembering
/// what it wired: the entity leaves the rebuild's query, so only the removal
/// observer can zero those ports.
#[test]
fn removing_the_declaration_silences_the_ports() {
    let mut app = app();
    let osc = spawn_node(&mut app, sine_hz::<f32>(440.0));
    let filt = spawn_node(&mut app, pass());
    let filt_id = node_id(&app, filt);

    app.world_mut()
        .entity_mut(filt)
        .insert(AudioSources::from(osc));
    app.update();
    assert_ne!(
        app.world().resource::<AudioGraphRes>().0.source(filt_id, 0),
        Source::Zero
    );

    app.world_mut().entity_mut(filt).remove::<AudioSources>();
    app.update();

    assert_eq!(
        app.world().resource::<AudioGraphRes>().0.source(filt_id, 0),
        Source::Zero,
        "a removed declaration must not leave its last wiring behind"
    );
}

/// A declaration naming an entity whose node arrives later is skipped, then
/// picked up — without anything about the declaration changing.
///
/// This is what `Added<AudioNode>` in the rebuild's dirty gate is for. Gating on
/// `Changed<AudioSources>` alone would leave the wire unformed forever.
#[test]
fn an_unresolvable_source_is_skipped_then_picked_up() {
    let mut app = app();
    let filt = spawn_node(&mut app, pass());
    let filt_id = node_id(&app, filt);

    // An entity with no node yet.
    let pending = app.world_mut().spawn_empty().id();
    app.world_mut()
        .entity_mut(filt)
        .insert(AudioSources::from(pending));
    app.update();
    assert_eq!(
        app.world().resource::<AudioGraphRes>().0.source(filt_id, 0),
        Source::Zero,
        "nothing to resolve yet, and no panic"
    );

    // The node turns up. The declaration is untouched.
    let id = {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        graph.0.add(sine_hz::<f32>(440.0))
    };
    app.world_mut().entity_mut(pending).insert(AudioNode(id));
    app.update();

    assert_eq!(
        app.world().resource::<AudioGraphRes>().0.source(filt_id, 0),
        Source::Local(id, 0),
        "the wire forms once the node exists — nothing about the declaration \
         changed, so a gate on `Changed<AudioSources>` alone would miss it"
    );
}

/// A node naming itself is skipped with a warning, not a panic.
///
/// `Net::set_source` asserts on a self-connection, and an assert inside a
/// reconcile system takes the app down over a caller's typo.
#[test]
fn a_self_connection_is_skipped_not_panicked_on() {
    let mut app = app();
    let filt = spawn_node(&mut app, pass());

    app.world_mut()
        .entity_mut(filt)
        .insert(AudioSources::from(filt));
    app.update();

    assert_eq!(
        app.world()
            .resource::<AudioGraphRes>()
            .0
            .source(node_id(&app, filt), 0),
        Source::Zero
    );
}

/// A source port past the node's output count is skipped rather than asserting.
#[test]
fn an_out_of_range_source_port_is_skipped() {
    let mut app = app();
    let mono = spawn_node(&mut app, sine_hz::<f32>(440.0));
    let filt = spawn_node(&mut app, pass());

    app.world_mut().entity_mut(filt).insert(
        AudioSources::silent().with(0, AudioSource::Node {
            entity: mono,
            port: 7,
        }),
    );
    app.update();

    assert_eq!(
        app.world()
            .resource::<AudioGraphRes>()
            .0
            .source(node_id(&app, filt), 0),
        Source::Zero
    );
}

/// A rebuild that finds the engine already agreeing writes nothing — it does not
/// re-set ports that already hold the declared source.
///
/// This is what the diff buys. `Net::set_source` calls `invalidate_order()`,
/// throwing away the cached topological sort, so re-writing an unchanged port is
/// not free; and marking `GraphDirty` forces a commit the frame did not need.
///
/// Driven by adding a *second, unrelated* node, which dirties the rebuild via
/// `Added<AudioNode>` without changing any existing declaration. Without the
/// diff, every already-correct port is rewritten and the frame is dirtied.
///
/// The flag has to be sampled **between** the rebuild and `commit_graph`, which
/// clears it in the `Commit` set — reading it after `update()` returns shows
/// `false` whether or not the rebuild wrote, which is how a first version of
/// this test passed against a deliberately un-diffed rebuild.
#[test]
fn a_rebuild_that_changes_nothing_writes_nothing() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let mut app = app();
    let osc = spawn_node(&mut app, sine_hz::<f32>(440.0));
    let filt = spawn_node(&mut app, pass());
    app.world_mut()
        .entity_mut(filt)
        .insert(AudioSources::from(osc));
    app.update();

    // Sample GraphDirty after the rebuild and before the commit clears it.
    let dirtied = Arc::new(AtomicBool::new(false));
    let probe = dirtied.clone();
    app.add_systems(
        Update,
        (move |dirty: Res<GraphDirty>| {
            if dirty.0 {
                probe.store(true, Ordering::SeqCst);
            }
        })
        .after(GraphReconcileSystems::Compensate)
        .before(GraphReconcileSystems::Commit),
    );

    // A new node elsewhere: the rebuild runs, but nothing it already wired has
    // changed.
    spawn_node(&mut app, sine_hz::<f32>(880.0));
    app.update();

    assert!(
        !dirtied.load(Ordering::SeqCst),
        "a rebuild whose declarations all match the engine must not dirty the \
         graph — re-writing a correct port discards the cached node order and \
         forces a commit the frame did not need"
    );
}
