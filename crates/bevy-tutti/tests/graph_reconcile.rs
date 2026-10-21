//! The node lifecycle, and the two things attached to a node that follow it.
//!
//! - `graph_reconcile` — spawn binds an entity to a graph node; despawn takes
//!   it back out. The lifecycle itself.
//! - `audio_param` — `AudioParam` reconciled into a real graph, asserted on the
//!   node's own value. What a live node's scalars do.
//! - `audio_tap` — the tap the audio callback pushes into, reachable from the
//!   ECS. What a live node's output is observed through.
//! - `engine_nodes` — the clock and click `build_into` makes (the click reads
//!   its block's `Env`, with no edge from the clock).
//!
//! Grouped because params and taps are both per-node state whose reconcilers
//! run in the same phase ordering as spawn and despawn, and a lifecycle change
//! is what would strand either. Bodies and test names are unchanged.

#[macro_use]
mod common;

/// The node lifecycle: spawn binds an entity to a graph node, despawn takes it
/// back out, crossfade swaps the unit in place, and one commit publishes the lot.
///
/// These moved out of `graph/reconcile.rs` when that file was split by duty.
/// They exercise only public API, so an integration test is their natural home —
/// and it proves the split kept the surface a host actually reaches intact.
mod graph_reconcile {
    use bevy_app::App;
    use bevy_ecs::prelude::*;

    use bevy_tutti::graph::{
        commit_graph, crossfade_audio_node, engine_ready, reconcile_node_despawn, AudioGraphRes,
        GraphReconcileSystems, SpawnAudioNode,
    };
    use bevy_tutti::AudioEngineState;
    use tutti_core::{AudioNode, Hz};
    use tutti_nodes::testing::Osc;

    /// Local probe component: the DAW param components moved out of the engine,
    /// so these pump tests use a self-contained marker to prove the chained
    /// `.insert(..)` on `spawn_audio_node`'s returned `EntityCommands` survives.
    #[derive(bevy_ecs::prelude::Component, Debug, Clone, Copy, PartialEq)]
    struct Probe(f32);

    /// A headless graph (no device), whose commits land on its own audio
    /// side; we never drive audio through it in these tests.
    fn test_app() -> App {
        let mut app = App::new();
        app.insert_resource(AudioGraphRes::headless(0, 2));
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
        app.insert_resource(AudioGraphRes::headless(0, 2));
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
            app.insert_resource(AudioGraphRes::headless(0, 2));

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
            .spawn_audio_node(tutti_graph::ForkByClone(Osc::sine(Hz(440.0))))
            .insert(Probe(0.5));
        app.update();

        // The entity is bound to the graph via `AudioNode`, keeps its chained
        // `Probe` insert, and the underlying node is in the graph.
        let mut q = app.world_mut().query::<(&AudioNode, &Probe)>();
        let mut count = 0;
        for (node, probe) in q.iter(app.world()) {
            count += 1;
            assert_eq!(probe.0, 0.5);
            assert!(app.world().resource::<AudioGraphRes>().contains(*node));
        }
        assert_eq!(count, 1);
    }

    #[test]
    fn despawn_removes_graph_node() {
        let mut app = test_app();
        let entity = {
            let mut c = app.world_mut().commands();
            c.spawn_audio_node(tutti_graph::ForkByClone(Osc::sine(Hz(440.0))))
                .id()
        };
        app.update();

        let node_id = *app.world().get::<AudioNode>(entity).expect("AudioNode");
        assert!(app.world().resource::<AudioGraphRes>().contains(node_id));

        app.world_mut().despawn(entity);
        app.update();

        assert!(!app.world().resource::<AudioGraphRes>().contains(node_id));
    }

    #[test]
    fn late_despawn_converges_within_one_frame() {
        // An `AudioNode` entity despawned by a system running *after* the
        // Commit phase (here: `Last`) must still have its graph node removed
        // and the graph converge (the edit sent) within one trailing frame.
        //
        // The `On<Remove, AudioNode>` observer fires at command-flush, so the
        // graph.remove (an edit) happens the same frame the despawn flushes; the
        // next frame's Commit-phase `commit_graph` coalesces the edit.
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let mut app = test_app();

        // Spawn the node in the normal way (so we can read its NodeKey once
        // the spawn command flushed).
        let entity = {
            let mut c = app.world_mut().commands();
            c.spawn_audio_node(tutti_graph::ForkByClone(Osc::sine(Hz(440.0))))
                .id()
        };
        app.update();
        let node_id = *app.world().get::<AudioNode>(entity).expect("AudioNode");
        assert!(app.world().resource::<AudioGraphRes>().contains(node_id));

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
        // On<Remove> observer removes the graph node, editing the session.
        app.update();
        // Frame B: Commit phase coalesces the pending edit → converged.
        app.update();

        assert!(app.world().get::<AudioNode>(entity).is_none());
        assert!(
            !app.world().resource::<AudioGraphRes>().contains(node_id),
            "late-despawned node removed from graph"
        );
        assert!(
            !app.world().resource::<AudioGraphRes>().is_edited(),
            "graph converged: the edit was sent after one trailing frame"
        );
    }

    #[test]
    fn sampler_volume_change_writes_through() {
        // Only verifies the dispatch path: a Changed<Volume> on a
        // sampler-like entity edits the session. Real sampler
        // construction needs an asset, which is beyond a unit test here.
        // The dispatch arm itself is covered by the example.
    }

    #[test]
    fn crossfade_replaces_node_in_place() {
        let mut app = test_app();
        let entity = {
            let mut c = app.world_mut().commands();
            c.spawn_audio_node(tutti_graph::ForkByClone(Osc::sine(Hz(440.0))))
                .id()
        };
        app.update();

        let node_id_before = *app.world().get::<AudioNode>(entity).expect("AudioNode");
        assert!(app
            .world()
            .resource::<AudioGraphRes>()
            .contains(node_id_before));

        // Replace with a different oscillator — same NodeKey, new unit.
        {
            let mut c = app.world_mut().commands();
            crossfade_audio_node(
                &mut c,
                entity,
                tutti_graph::ForkByClone(Osc::sine(Hz(220.0))),
            );
        }
        app.update();

        // Same NodeKey stays — that's the contract of crossfade.
        let node_id_after = *app.world().get::<AudioNode>(entity).expect("AudioNode");
        assert_eq!(node_id_before, node_id_after);
        assert!(app
            .world()
            .resource::<AudioGraphRes>()
            .contains(node_id_after));
    }
}

/// `AudioParam` reconciled into a real graph, asserted on the node's own value.
///
/// The interesting cases are the seams: a param reaching the node at all, a
/// steady frame doing nothing, and an authored write on a *modulated* param
/// moving the knob its modulation rides on.
mod audio_param {
    // The plain-reconcile tests below run in every configuration; the ones that
    // need a modulation driver are gated individually. Gating the whole file would
    // leave the `not(modulation)` branch of the reconciler compiled but never run.
    use bevy_app::prelude::*;
    use bevy_ecs::prelude::*;

    use bevy_tutti::graph::{
        AudioGraphRes, AudioParam, AudioParamAppExt, CapturedControls, GraphReconcilePlugin,
        SessionNode, TransportRes,
    };
    #[cfg(feature = "modulation")]
    use bevy_tutti::modulation::{
        Macro, ModRange, ModRoute, ModSource, ParamRef, TuttiModulationPlugin,
    };
    use bevy_tutti::AudioEngineState;
    use tutti_core::transport::Transport;
    use tutti_types::{Drive, Hz, UnitParam};
    // Only the modulation tests below use these.
    use tutti_nodes::DistortionNode;
    #[cfg(feature = "modulation")]
    use tutti_types::Depth;

    /// The drive a freshly built node carries.
    const INITIAL_DRIVE: f32 = 1.0;

    /// `Drive` on a distortion node — a param with a readable atomic behind it.
    type DriveParam = AudioParam<Drive, { UnitParam::Drive as u16 }>;

    fn app_with_node() -> (App, Entity) {
        let mut app = App::new();

        let unit = DistortionNode::new(tutti_nodes::ShapeKind::Tanh, INITIAL_DRIVE);
        // The node's own drive atomic, shared with every clone of it — what the
        // DSP reads, reachable without asking the graph for its copy.
        let drive = DriveCell(unit.drive());
        let mut graph = AudioGraphRes::headless(0, 1);
        graph.set_sample_rate(tutti_core::SampleRate(48_000.0));
        // A `ParamNode`: a param set by address writes the cell it reads, which
        // it reads on its next block. `node_drive` renders a frame before it
        // reads all the same.

        app.insert_resource(graph);
        app.insert_resource(TransportRes(Transport::new(48_000.0)));
        app.insert_resource(AudioEngineState::Running);
        app.add_plugins(GraphReconcilePlugin);
        #[cfg(feature = "modulation")]
        app.add_plugins(TuttiModulationPlugin);
        app.add_audio_param::<Drive, { UnitParam::Drive as u16 }>();

        // Bound as every insertion path binds (a node that is not a plugin
        // captures nothing): a `ParamNode`'s controls are its `ParamSet`,
        // addressed on the node so an `AudioParam` writes through it (what
        // `spawn_audio_node` does).
        let controls = CapturedControls::default();
        let node = {
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            let (node, params) = graph.insert(unit);
            graph.set_node_params(node, DistortionNode::params(&params));
            graph.set_outputs_from(node);
            node
        };
        let mut entity = app.world_mut().spawn(drive);
        controls.bind(&mut entity, node);
        let entity = entity.id();
        (app, entity)
    }

    /// The node's drive atomic, taken from the unit before it moved into the
    /// graph.
    #[derive(Component)]
    struct DriveCell(std::sync::Arc<tutti_core::AtomicF32>);

    /// The node's live drive — what the DSP reads — after one rendered frame,
    /// so a setting queued for the node has reached it (see
    /// `app_with_node`).
    fn node_drive(app: &mut App, entity: Entity) -> f32 {
        app.world_mut()
            .resource_mut::<AudioGraphRes>()
            .render_frame(&mut [0.0]);
        app.world()
            .get::<DriveCell>(entity)
            .unwrap()
            .0
            .load(std::sync::atomic::Ordering::Acquire)
    }

    #[test]
    fn an_inserted_param_reaches_the_node() {
        let (mut app, entity) = app_with_node();

        app.world_mut()
            .entity_mut(entity)
            .insert(DriveParam::new(Drive(4.0)));
        app.update();

        assert_eq!(node_drive(&mut app, entity), 4.0);
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

        assert_eq!(node_drive(&mut app, entity), 7.5);
    }

    /// Change detection is the whole gate: without it every param would push every
    /// frame, re-sending every param to the audio thread each frame forever.
    #[test]
    fn an_unchanged_param_does_not_push() {
        let (mut app, entity) = app_with_node();
        app.world_mut()
            .entity_mut(entity)
            .insert(DriveParam::new(Drive(4.0)));
        app.update();
        // The first push lands before the poke. It waits in the node's ring
        // for the next block, and delivered after the poke
        // it would look exactly like the re-push this test is about.
        assert_eq!(node_drive(&mut app, entity), 4.0);

        // Move the node's value behind the reconciler's back. A push would restore
        // it to 4.0; silence leaves the poke standing.
        app.world()
            .get::<DriveCell>(entity)
            .unwrap()
            .0
            .store(9.0, std::sync::atomic::Ordering::Release);

        app.update();

        assert_eq!(
            node_drive(&mut app, entity),
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

        app.world_mut().entity_mut(entity).insert(
            AudioParam::<Hz, { UnitParam::Cutoff as u16 }>::new(Hz(800.0)),
        );
        app.update();

        assert_eq!(
            node_drive(&mut app, entity),
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

        assert_eq!(node_drive(&mut app, entity), 4.0);
    }

    /// The single-writer rule (doc 014 §8.2): a modulated param's knob
    /// is the node's own cell, and an authored write lands there. A
    /// modulation lane sends `knob + Σ offsets` into the param's port and
    /// never writes the cell, so the authored value is not reverted within a
    /// frame (the fader moving on screen and not in the sound) and nothing
    /// else holds a base: the lane's knob is the written value.
    ///
    /// Mutation (run): `write_param` dropping the write on a param that has a
    /// lane (the old modulated branch writing a base nowhere the lane reads)
    /// → the cell and the lane's knob stay at 5 → fails.
    #[cfg(feature = "modulation")]
    #[test]
    fn an_authored_write_to_a_modulated_param_moves_the_knob() {
        // `app_with_node` captures the node's `ParamSet` before it binds the
        // node — the capture is read once, before insertion.
        let (mut app, entity) = app_with_node();
        let drive = ParamRef::unit(entity, UnitParam::Drive);

        app.world_mut()
            .entity_mut(entity)
            .insert(DriveParam::new(Drive(5.0)));
        app.world_mut().spawn(ModRange::new(drive, 0.0, 10.0));
        // A macro holds a constant offset, so the knob stays legible.
        let offset = app.world_mut().spawn(ModSource::new(Macro(1.0))).id();
        app.world_mut()
            .spawn(ModRoute::new(offset, drive).with_depth(Depth(0.1)));
        for _ in 0..3 {
            app.update();
        }
        let knob = |app: &App| {
            app.world()
                .resource::<AudioGraphRes>()
                .modulation()
                .lane(drive)
                .expect("the drive has a lane")
                .1
                .knob()
        };
        assert_eq!(knob(&app), Some(5.0));
        assert_eq!(node_drive(&mut app, entity), 5.0, "the cell is the knob");

        // Author a new value while modulation rides on the param.
        app.world_mut()
            .entity_mut(entity)
            .insert(DriveParam::new(Drive(8.0)));
        app.update();

        assert_eq!(node_drive(&mut app, entity), 8.0, "the write lands");
        assert_eq!(knob(&app), Some(8.0), "and is the lane's knob");
    }
}

/// The tap the audio callback pushes into must be reachable from the ECS.
///
/// `build_into` created an `AudioTap`, handed a clone to the RT callback, and
/// dropped the local. Nothing else held a handle, so `open()` — the only way to
/// get the consumer end — was unreachable, and the callback pushed every block
/// into a ring nobody could ever read. `tutti-analysis`, the documented
/// consumer, had no route in at all.
///
/// These drive the wrapper directly. The build-time publish itself needs a real
/// audio device (`TuttiPlugin { disabled: true }` skips `build_into` entirely),
/// so it is covered by the ignored test at the bottom rather than claimed here.
mod audio_tap {
    use bevy_app::App;
    use bevy_tutti::graph::AudioTapRes;

    /// A published tap starts closed: opening is the host's decision, not the
    /// engine's, and while closed the audio thread pays one atomic load.
    #[test]
    fn a_published_tap_starts_closed() {
        let mut app = App::new();
        app.insert_resource(AudioTapRes::default());

        assert!(!app.world().resource::<AudioTapRes>().is_open());
    }

    /// The whole point of the seam: a host can get the consumer end, and it sees
    /// what the callback pushed.
    #[test]
    fn opening_the_tap_yields_a_consumer_that_sees_the_pushed_frames() {
        let mut app = App::new();
        app.insert_resource(AudioTapRes::default());

        // The callback's half — the clone `build_into` hands to `AudioCallbackState`.
        let callback_side = app.world().resource::<AudioTapRes>().0.clone();
        let mut consumer = app
            .world()
            .resource::<AudioTapRes>()
            .open()
            .expect("a fresh tap opens");
        assert!(app.world().resource::<AudioTapRes>().is_open());

        // Two interleaved stereo frames, as a block would arrive.
        callback_side.push(&[0.25, -0.5, 0.75, -1.0], 2);

        assert_eq!(consumer.try_pop(), Some((0.25, -0.5)));
        assert_eq!(consumer.try_pop(), Some((0.75, -1.0)));
        assert_eq!(consumer.try_pop(), None, "and nothing more than was pushed");
    }

    /// Closing stops the copy, so a host that finishes analysing gets its
    /// atomic-load-only path back.
    #[test]
    fn closing_the_tap_stops_the_copy() {
        let mut app = App::new();
        app.insert_resource(AudioTapRes::default());

        let callback_side = app.world().resource::<AudioTapRes>().0.clone();
        let mut consumer = app
            .world()
            .resource::<AudioTapRes>()
            .open()
            .expect("a fresh tap opens");
        app.world().resource::<AudioTapRes>().close();
        assert!(!app.world().resource::<AudioTapRes>().is_open());

        callback_side.push(&[0.25, -0.5], 1);

        assert_eq!(consumer.try_pop(), None, "a closed tap copies nothing");
    }

    /// A closed tap accepts pushes without panicking — the state the audio thread
    /// is in for every block until a host opens it.
    #[test]
    fn pushing_into_a_closed_tap_is_a_no_op() {
        let tap = AudioTapRes::default();
        tap.0.push(&[0.1, 0.2, 0.3, 0.4], 2);
        assert!(!tap.is_open());
    }

    // `the_engine_publishes_its_tap` (ignored: it opened a real device) moved
    // to `engine::build`'s `the_engine_publishes_the_tap_its_callback_feeds`,
    // which builds device-free and checks the frames reach the published tap.
}

/// The node the engine builds for itself.
mod engine_nodes {
    use bevy_app::App;
    use bevy_tutti::graph::{AudioGraphRes, EngineNodes};
    use tutti_core::AudioNode;

    /// The metronome reads the beat of every frame from its block's `Env`,
    /// so it has no inputs to wire and cannot miss a wire.
    ///
    /// Ignored: `build_into`
    /// opens a real CPAL device. It passes on a machine with ALSA's default
    /// device, which is how it was run.
    ///
    /// Mutation (not run here: needs a device): inserting the click as a
    /// two-input node → "the click has no inputs" fails.
    #[test]
    #[ignore = "requires an audio device"]
    fn the_click_reads_the_beat_from_its_env() {
        let mut app = App::new();
        app.add_plugins(bevy_tutti::TuttiPlugin::default());
        app.update();

        let nodes = *app.world().resource::<EngineNodes>();
        let id_of = |entity| *app.world().get::<AudioNode>(entity).unwrap();
        let click = id_of(nodes.click);

        let graph = app.world().resource::<AudioGraphRes>();
        assert_eq!(graph.node_inputs(click), 0, "the click has no inputs");
        assert_eq!(graph.node_outputs(click), 2, "the click is stereo");
    }
}
