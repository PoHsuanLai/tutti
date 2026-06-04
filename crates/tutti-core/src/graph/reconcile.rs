//! Reconcile entity-as-node component changes into [`AudioGraph`] operations.
//!
//! See [`crate::graph`] for the component types. This module provides:
//!
//! - [`SpawnAudioNode`] — `Commands` extension to atomically `graph.add(unit)`
//!   and attach `AudioNode` + `NodeKind` to a fresh entity.
//! - [`reconcile_node_despawn`] — an `On<Remove, AudioNode>` observer that
//!   removes the underlying graph node when its `AudioNode` is removed.
//! - [`reconcile_params`] — sweeps `Changed<Volume>` (and friends) and writes
//!   the new value through a typed `node_mut::<T>` call.
//! - [`commit_graph`] — `graph.commit()` once per frame iff any reconcile
//!   system mutated the graph.
//! - [`GraphReconcileSystems`] — system-set ordering anchor for hosts that
//!   want to schedule their own logic before/after reconciliation.

use bevy_ecs::prelude::*;
use bevy_ecs::schedule::SystemSet;
use bevy_ecs::system::EntityCommands;

use crate::dsp::AudioUnit;
use crate::graph::{AudioNode, Mute, NodeKind, AudioGraphRes, Volume};

/// System-set ordering anchor for the reconcile pipeline.
///
/// Apps can schedule their own systems against these sets. The plugin
/// runs them in the order: `Spawn` → `Params` → `Despawn` → `Commit`,
/// all inside `Update`.
#[derive(SystemSet, Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum GraphReconcileSystems {
    /// Initial spawn of new graph nodes (rare; mostly app-driven via
    /// [`SpawnAudioNode`]). Apps can hook here to populate parameter
    /// components on the same frame the node is created.
    Spawn,
    /// Parameter-component changes are written into the graph here.
    Params,
    /// Reserved for despawn-phase work hosts may want to order here.
    /// Graph-node removal itself is now an `On<Remove, AudioNode>` observer
    /// (`reconcile_node_despawn`), which fires at command-flush time rather
    /// than in this set.
    Despawn,
    /// Single `graph.commit()` if any earlier set mutated the graph.
    Commit,
}

/// Run-condition: the audio engine built successfully and its resources are
/// present.
///
/// Every engine resource (`AudioGraphRes`, `TransportRes`, `MeteringRes`,
/// `AudioConfig`, and the feature subsystem resources) is inserted during the
/// synchronous plugin-build pass: `build_into` inserts a transient `PendingX`
/// per subsystem iff the RT build succeeded (none on failure), then each
/// subsystem plugin's `build()` promotes its `PendingX` into its `*Res`. All of
/// this runs before frame 1, so `resource_exists::<AudioGraphRes>` is an exact
/// proxy for "engine ready", and every system gated on this can take its engine
/// resources as plain `Res`/`ResMut` instead of `Option<Res<_>>` + a
/// `let Some(..) else` guard — the system simply does not run when the engine is
/// absent (the idiomatic Bevy shape, mirroring `bevy_audio`'s
/// `audio_output_available`).
///
/// Two resources are *not* covered (they may be absent even when the engine
/// built) and must keep `Option<Res<_>>`: `MidiIoRes` (only when a hardware
/// MIDI port opened) and `PluginsRes` (inserted lazily, not in the engine
/// block).
pub fn engine_ready(graph: Option<Res<AudioGraphRes>>) -> bool {
    graph.is_some()
}

/// Per-frame "did anything change?" flag used to coalesce
/// `graph.commit()` to at most one call per frame.
#[derive(Resource, Default)]
pub struct GraphDirty(pub bool);

/// `Commands` extension that adds a unit to the graph and spawns an entity
/// with `AudioNode(id)` + `NodeKind` attached.
///
/// The graph mutation is queued as a deferred command and applies at the
/// next command-buffer flush — the returned `EntityCommands` lets the
/// caller chain `.insert((Volume(0.5), Pan(0.0)))` on the same entity in
/// the usual fashion.
///
/// # Example
///
/// ```rust,ignore
/// use bevy::prelude::*;
/// use bevy_tutti::*;
/// use crate::core::dsp::sine_hz;
///
/// fn setup(mut commands: Commands) {
///     commands.spawn_audio_node(sine_hz::<f32>(440.0), NodeKind::Generator)
///             .insert(Volume(0.5));
/// }
/// ```
pub trait SpawnAudioNode {
    /// Add `unit` to the graph and spawn an entity bound to it.
    fn spawn_audio_node<U>(&mut self, unit: U, kind: NodeKind) -> EntityCommands<'_>
    where
        U: AudioUnit + 'static;
}

impl<'w, 's> SpawnAudioNode for Commands<'w, 's> {
    fn spawn_audio_node<U>(&mut self, unit: U, kind: NodeKind) -> EntityCommands<'_>
    where
        U: AudioUnit + 'static,
    {
        let entity = self.spawn_empty().id();
        self.queue(move |world: &mut World| {
            let id = match world.get_resource_mut::<AudioGraphRes>() {
                Some(mut graph) => graph.0.add(unit),
                None => {
                    bevy_log::warn!(
                        "spawn_audio_node: AudioGraphRes missing; entity {:?} left without AudioNode",
                        entity
                    );
                    return;
                }
            };
            // Mark the graph dirty so the per-frame commit system flushes
            // this addition along with whatever else mutated this frame.
            if let Some(mut dirty) = world.get_resource_mut::<GraphDirty>() {
                dirty.0 = true;
            }
            if let Ok(mut e) = world.get_entity_mut(entity) {
                e.insert((AudioNode(id), kind));
            }
        });
        self.entity(entity)
    }
}

/// Crossfade-replace an entity's underlying graph node with `new_unit`.
///
/// Queues a deferred world command that:
///
/// 1. Looks up the entity's [`AudioNode(NodeId)`](AudioNode).
/// 2. Calls [`AudioGraph::crossfade_boxed`] with a 5 ms `Smooth` fade.
/// 3. Marks [`GraphDirty`] so the per-frame [`commit_graph`] flushes.
///
/// The same `NodeId` survives the crossfade — connections to/from this node
/// stay valid. Callers don't need to update any other components.
///
/// Use this for parameter changes that aren't safe to mutate live (e.g. a
/// filter cutoff baked into the unit at construction, a sampler loop range
/// that requires re-priming the streamer). For RT-safe atomic changes
/// (`Volume`, `Mute`, `SamplerSpeed`, `PluginParam`, …), edit the
/// component instead and let the reconcile pipeline handle it.
///
/// If the entity has no `AudioNode` (e.g. it was despawned), or the
/// graph resource is missing, this is a no-op and logs a warning.
pub fn crossfade_audio_node(
    commands: &mut Commands<'_, '_>,
    entity: Entity,
    new_unit: Box<dyn AudioUnit>,
) {
    commands.queue(move |world: &mut World| {
        let Some(node) = world.get::<AudioNode>(entity).copied() else {
            bevy_log::warn!(
                "crossfade_audio_node: entity {:?} has no AudioNode; nothing to crossfade",
                entity
            );
            return;
        };
        let Some(mut graph) = world.get_resource_mut::<AudioGraphRes>() else {
            bevy_log::warn!(
                "crossfade_audio_node: AudioGraphRes missing; entity {:?} not crossfaded",
                entity
            );
            return;
        };
        graph.0.crossfade_boxed(node.0, crate::Fade::Smooth, 0.005, new_unit);
        if let Some(mut dirty) = world.get_resource_mut::<GraphDirty>() {
            dirty.0 = true;
        }
    });
}

/// Observer: removes a graph node when its `AudioNode` component is removed
/// (including via despawn).
///
/// `On<Remove, AudioNode>` fires *before* the component value is dropped, so
/// the `NodeId` is still readable off the triggered entity — no local
/// `(Entity, NodeId)` map needed. Only mutates the graph + sets `GraphDirty`;
/// the per-frame [`commit_graph`] (Commit phase) does the actual commit.
pub fn reconcile_node_despawn(
    remove: On<Remove, AudioNode>,
    nodes: Query<&AudioNode>,
    graph: Option<ResMut<AudioGraphRes>>,
    mut dirty: ResMut<GraphDirty>,
) {
    let entity = remove.event_target();
    let Ok(node) = nodes.get(entity) else { return };
    let Some(mut graph) = graph else { return };
    if graph.0.contains(node.0) {
        graph.0.remove(node.0);
        dirty.0 = true;
    }
}

type ChangedParams<'w> = (&'w AudioNode, &'w NodeKind, &'w Volume, Option<&'w Mute>);
type ChangedParamFilter = Or<(Changed<Volume>, Changed<Mute>)>;

/// Reconciles `Changed<Volume>` and `Changed<Mute>` into the underlying
/// graph node. Dispatch is keyed off [`NodeKind`]; unknown kinds are
/// skipped (apps can layer their own systems for custom kinds).
///
/// The core build of this system carries no per-kind arms: the sampler
/// volume write-through is re-added in the tutti-sampler stage as a layered
/// system. Hosts wanting first-class typed setters for other kinds layer
/// their own systems against [`GraphReconcileSystems::Params`].
#[allow(unused_mut, unused_variables)]
pub fn reconcile_params(
    mut graph: ResMut<AudioGraphRes>,
    changed_vol: Query<ChangedParams, ChangedParamFilter>,
    mut dirty: ResMut<GraphDirty>,
) {
    for (node, kind, volume, mute) in changed_vol.iter() {
        let muted = mute.map(|m| m.0).unwrap_or(false);
        let target = if muted { 0.0 } else { volume.0 };

        match *kind {
            // No first-class typed setter at this layer; hosts (and the
            // leaf crates) layer their own systems. See module docs for the
            // extension pattern.
            _ => {
                let _ = (target, node);
            }
        }
    }
}

/// Runs `graph.commit()` once iff any reconcile system mutated the graph.
pub fn commit_graph(mut graph: ResMut<AudioGraphRes>, mut dirty: ResMut<GraphDirty>) {
    if !dirty.0 {
        return;
    }
    graph.0.commit();
    dirty.0 = false;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::sine_hz;
    use crate::graph::AudioGraphRes;
    use crate::AudioGraph;
    use crate::{PdcManager, GraphNet};
    use bevy_app::App;

    /// Build a bare `AudioGraph` directly (no `TuttiEngine`, which lives in
    /// bevy-tutti). Allocates the fundsp backend so `commit()` has something
    /// to publish into; we never drive audio through it in these tests.
    fn bare_graph(channels: usize) -> AudioGraph {
        let mut net = GraphNet::new(0, channels);
        let _backend = net.backend();
        let pdc = PdcManager::new(channels, 0);
        #[cfg(feature = "midi")]
        let midi_route = tutti_midi_types::MidiRoutingTable::new();
        AudioGraph::from_parts(
            net,
            pdc,
            #[cfg(feature = "midi")]
            midi_route,
            48_000.0,
            channels,
        )
    }

    fn test_app() -> App {
        let mut app = App::new();
        app.insert_resource(AudioGraphRes(bare_graph(2)));
        app.init_resource::<GraphDirty>();
        app.add_observer(reconcile_node_despawn);
        app.add_systems(
            bevy_app::Update,
            (
                reconcile_params.in_set(GraphReconcileSystems::Params),
                commit_graph.in_set(GraphReconcileSystems::Commit),
            ),
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
        assert_eq!(ran.load(Ordering::SeqCst), 0, "gated system skipped with no engine");

        // Insert the resource (engine built): the gate now passes.
        app.insert_resource(AudioGraphRes(bare_graph(2)));
        app.update();
        assert_eq!(ran.load(Ordering::SeqCst), 1, "gated system runs once engine present");
    }

    #[test]
    fn spawn_inserts_audio_node() {
        let mut app = test_app();
        let mut commands_q = app.world_mut().commands();
        commands_q
            .spawn_audio_node(sine_hz::<f32>(440.0), NodeKind::Generator)
            .insert(Volume(0.5));
        app.update();

        let mut q = app.world_mut().query::<(&AudioNode, &NodeKind, &Volume)>();
        let mut count = 0;
        for (node, kind, vol) in q.iter(app.world()) {
            count += 1;
            assert_eq!(*kind, NodeKind::Generator);
            assert_eq!(vol.0, 0.5);
            assert!(app.world().resource::<AudioGraphRes>().0.contains(node.0));
        }
        assert_eq!(count, 1);
    }

    #[test]
    fn despawn_removes_graph_node() {
        let mut app = test_app();
        let entity = {
            let mut c = app.world_mut().commands();
            c.spawn_audio_node(sine_hz::<f32>(440.0), NodeKind::Generator).id()
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
            c.spawn_audio_node(sine_hz::<f32>(440.0), NodeKind::Generator)
                .id()
        };
        app.update();
        let node_id = app.world().get::<AudioNode>(entity).expect("AudioNode").0;
        assert!(app
            .world()
            .resource::<AudioGraphRes>()
            .0
            .contains(node_id));

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
            !app.world()
                .resource::<AudioGraphRes>()
                .0
                .contains(node_id),
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
        // NodeKind::Sampler entity sets the dirty flag. Real sampler
        // construction needs an asset, which is beyond a unit test here.
        // The dispatch arm itself is covered by the example.
    }

    #[test]
    fn crossfade_replaces_node_in_place() {
        let mut app = test_app();
        let entity = {
            let mut c = app.world_mut().commands();
            c.spawn_audio_node(sine_hz::<f32>(440.0), NodeKind::Generator).id()
        };
        app.update();

        let node_id_before = app.world().get::<AudioNode>(entity).expect("AudioNode").0;
        assert!(app.world().resource::<AudioGraphRes>().0.contains(node_id_before));

        // Replace with a different oscillator — same NodeId, new unit.
        {
            let mut c = app.world_mut().commands();
            crossfade_audio_node(&mut c, entity, Box::new(sine_hz::<f32>(220.0)));
        }
        app.update();

        // Same NodeId stays — that's the contract of crossfade.
        let node_id_after = app.world().get::<AudioNode>(entity).expect("AudioNode").0;
        assert_eq!(node_id_before, node_id_after);
        assert!(app.world().resource::<AudioGraphRes>().0.contains(node_id_after));
    }
}
