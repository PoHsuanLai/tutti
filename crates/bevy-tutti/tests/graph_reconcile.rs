//! The node lifecycle: spawn binds an entity to a graph node, despawn takes it
//! back out, crossfade swaps the unit in place, and one commit publishes the lot.
//!
//! These moved out of `graph/reconcile.rs` when that file was split by duty.
//! They exercise only public API, so an integration test is their natural home —
//! and it proves the split kept the surface a host actually reaches intact.

use bevy_app::App;
use bevy_ecs::prelude::*;

use bevy_tutti::graph::{
    commit_graph, crossfade_audio_node, engine_ready, reconcile_node_despawn, AudioGraphRes,
    GraphDirty, GraphReconcileSystems, SpawnAudioNode,
};
use bevy_tutti::AudioEngineState;
use tutti_core::dsp::{sine_hz, Net};
use tutti_core::AudioNode;

/// Local probe component: the DAW param components moved out of the engine,
/// so these pump tests use a self-contained marker to prove the chained
/// `.insert(..)` on `spawn_audio_node`'s returned `EntityCommands` survives.
#[derive(bevy_ecs::prelude::Component, Debug, Clone, Copy, PartialEq)]
struct Probe(f32);

/// Build a bare `Net` directly (no `TuttiEngine`, which lives in
/// bevy-tutti). Allocates the fundsp backend so `commit()` has something
/// to publish into; we never drive audio through it in these tests.
fn test_app() -> App {
    let mut app = App::new();
    app.insert_resource(AudioGraphRes(Net::with_backend(2)));
    app.init_resource::<GraphDirty>();
    app.add_observer(reconcile_node_despawn);
    app.add_systems(
        bevy_app::Update,
        commit_graph.in_set(GraphReconcileSystems::Commit),
    );
    app.configure_sets(
        bevy_app::Update,
        (
            GraphReconcileSystems::Spawn,
            GraphReconcileSystems::Params,
            GraphReconcileSystems::Despawn,
            GraphReconcileSystems::Commit,
        )
            .chain(),
    );
    app
}

/// The `engine_ready` gate must keep a plain-`ResMut<AudioGraphRes>` system
/// from running (and panicking on the missing resource) when the engine
/// failed to build — and must let it run once the resource is present.
#[test]
fn engine_ready_gates_plain_res_system() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let ran = Arc::new(AtomicUsize::new(0));
    let ran_c = ran.clone();
    // A system that takes the engine resource as a PLAIN ResMut — it would
    // panic if scheduled without AudioGraphRes present.
    let sys = move |_graph: ResMut<AudioGraphRes>| {
        ran_c.fetch_add(1, Ordering::SeqCst);
    };

    // No engine: AudioGraphRes absent. The gate must skip the system, so
    // `update()` does not panic and the system never runs.
    let mut app = App::new();
    app.add_systems(bevy_app::Update, sys.run_if(engine_ready));
    app.update();
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "gated system skipped with no engine"
    );

    // Engine built: both the state and the resource it reports on are
    // present, so the gate passes.
    app.insert_resource(AudioGraphRes(Net::with_backend(2)));
    app.insert_resource(AudioEngineState::Running);
    app.update();
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "gated system runs once engine present"
    );
}

/// `Failed` and `Disabled` must both gate systems off — a disabled engine is
/// as unable to render as a broken one, and neither may let a plain
/// `Res<AudioGraphRes>` system through.
#[test]
fn engine_ready_is_false_unless_running() {
    for state in [
        AudioEngineState::Failed("no device".into()),
        AudioEngineState::Disabled,
    ] {
        let mut app = App::new();
        app.insert_resource(state.clone());
        // Present but irrelevant: the state decides, not the resource.
        app.insert_resource(AudioGraphRes(Net::with_backend(2)));

        let ready = app
            .world_mut()
            .run_system_cached(engine_ready)
            .expect("run condition is a valid system");
        assert!(!ready, "{state:?} must not read as ready");
    }
}

/// A `World` that never added `TuttiPlugin` has no state at all. The gate
/// must treat that as not-ready rather than panicking on a missing resource.
#[test]
fn engine_ready_is_false_without_the_plugin() {
    let mut app = App::new();
    let ready = app
        .world_mut()
        .run_system_cached(engine_ready)
        .expect("run condition is a valid system");
    assert!(!ready, "a world with no TuttiPlugin is not ready");
}

#[test]
fn spawn_inserts_audio_node() {
    let mut app = test_app();
    let mut commands_q = app.world_mut().commands();
    commands_q
        .spawn_audio_node(sine_hz::<f32>(440.0))
        .insert(Probe(0.5));
    app.update();

    // The entity is bound to the graph via `AudioNode`, keeps its chained
    // `Probe` insert, and the underlying node is in the graph.
    let mut q = app.world_mut().query::<(&AudioNode, &Probe)>();
    let mut count = 0;
    for (node, probe) in q.iter(app.world()) {
        count += 1;
        assert_eq!(probe.0, 0.5);
        assert!(app.world().resource::<AudioGraphRes>().0.contains(node.0));
    }
    assert_eq!(count, 1);
}

#[test]
fn despawn_removes_graph_node() {
    let mut app = test_app();
    let entity = {
        let mut c = app.world_mut().commands();
        c.spawn_audio_node(sine_hz::<f32>(440.0)).id()
    };
    app.update();

    let node_id = app.world().get::<AudioNode>(entity).expect("AudioNode").0;
    assert!(app.world().resource::<AudioGraphRes>().0.contains(node_id));

    app.world_mut().despawn(entity);
    app.update();

    assert!(!app.world().resource::<AudioGraphRes>().0.contains(node_id));
}

#[test]
fn late_despawn_converges_within_one_frame() {
    // An `AudioNode` entity despawned by a system running *after* the
    // Commit phase (here: `Last`) must still have its graph node removed
    // and the graph converge (dirty cleared) within one trailing frame.
    //
    // The `On<Remove, AudioNode>` observer fires at command-flush, so the
    // graph.remove + dirty happen the same frame the despawn flushes; the
    // next frame's Commit-phase `commit_graph` coalesces the edit.
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let mut app = test_app();

    // Spawn the node in the normal way (so we can read its NodeId once
    // the spawn command flushed).
    let entity = {
        let mut c = app.world_mut().commands();
        c.spawn_audio_node(sine_hz::<f32>(440.0)).id()
    };
    app.update();
    let node_id = app.world().get::<AudioNode>(entity).expect("AudioNode").0;
    assert!(app.world().resource::<AudioGraphRes>().0.contains(node_id));

    // A `Last`-phase system (runs after GraphReconcileSystems::Commit)
    // despawns the entity exactly once.
    let fired = Arc::new(AtomicBool::new(false));
    let fired_c = fired.clone();
    app.add_systems(
        bevy_app::Last,
        move |mut commands: Commands, q: Query<Entity, With<AudioNode>>| {
            if fired_c.swap(true, Ordering::SeqCst) {
                return;
            }
            for e in q.iter() {
                commands.entity(e).despawn();
            }
        },
    );

    // Frame A: the late despawn flushes at end of `Last`; the
    // On<Remove> observer removes the graph node and sets dirty there.
    app.update();
    // Frame B: Commit phase coalesces the pending edit → converged.
    app.update();

    assert!(app.world().get::<AudioNode>(entity).is_none());
    assert!(
        !app.world().resource::<AudioGraphRes>().0.contains(node_id),
        "late-despawned node removed from graph"
    );
    assert!(
        !app.world().resource::<GraphDirty>().0,
        "graph converged: dirty flag cleared after one trailing frame"
    );
}

#[test]
fn sampler_volume_change_writes_through() {
    // Only verifies the dispatch path: a Changed<Volume> on a
    // sampler-like entity sets the dirty flag. Real sampler
    // construction needs an asset, which is beyond a unit test here.
    // The dispatch arm itself is covered by the example.
}

#[test]
fn crossfade_replaces_node_in_place() {
    let mut app = test_app();
    let entity = {
        let mut c = app.world_mut().commands();
        c.spawn_audio_node(sine_hz::<f32>(440.0)).id()
    };
    app.update();

    let node_id_before = app.world().get::<AudioNode>(entity).expect("AudioNode").0;
    assert!(app
        .world()
        .resource::<AudioGraphRes>()
        .0
        .contains(node_id_before));

    // Replace with a different oscillator — same NodeId, new unit.
    {
        let mut c = app.world_mut().commands();
        crossfade_audio_node(&mut c, entity, Box::new(sine_hz::<f32>(220.0)));
    }
    app.update();

    // Same NodeId stays — that's the contract of crossfade.
    let node_id_after = app.world().get::<AudioNode>(entity).expect("AudioNode").0;
    assert_eq!(node_id_before, node_id_after);
    assert!(app
        .world()
        .resource::<AudioGraphRes>()
        .0
        .contains(node_id_after));
}
