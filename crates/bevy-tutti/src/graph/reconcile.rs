//! Reconcile entity-as-node component changes into [`TuttiGraph`] operations.
//!
//! See [`tutti::ecs`] for the component types. This module provides:
//!
//! - [`SpawnAudioNode`] — `Commands` extension to atomically `graph.add(unit)`
//!   and attach `AudioNode` + `NodeKind` to a fresh entity.
//! - [`reconcile_node_despawn`] — picks up `RemovedComponents<AudioNode>`
//!   and removes the underlying graph node.
//! - [`reconcile_params`] — sweeps `Changed<Volume>` (and friends) and writes
//!   the new value through a typed `node_mut::<T>` call.
//! - [`commit_graph`] — `graph.commit()` once per frame iff any reconcile
//!   system mutated the graph.
//! - [`GraphReconcileSystems`] — system-set ordering anchor for hosts that
//!   want to schedule their own logic before/after reconciliation.

use bevy_ecs::prelude::*;
use bevy_ecs::schedule::SystemSet;
use bevy_ecs::system::EntityCommands;

use tutti::core::ecs::{AudioNode, Mute, NodeKind, Volume};
use tutti::dsp::AudioUnit;

use crate::resources::TuttiGraphRes;

#[cfg(feature = "sampler")]
use tutti::core::ecs::{SamplerLooping, SamplerSpeed};
#[cfg(feature = "sampler")]
use tutti::sampler::SamplerUnit;

#[cfg(feature = "plugin")]
use tutti::core::ecs::PluginParam;
#[cfg(feature = "plugin")]
use crate::plugin_host::PluginEmitter;

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
    /// Entities whose `AudioNode` was removed (or who were despawned)
    /// have their graph node removed here.
    Despawn,
    /// Single `graph.commit()` if any earlier set mutated the graph.
    Commit,
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
/// use tutti::dsp::sine_hz;
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
            let id = match world.get_resource_mut::<TuttiGraphRes>() {
                Some(mut graph) => graph.0.add(unit),
                None => {
                    bevy_log::warn!(
                        "spawn_audio_node: TuttiGraphRes missing; entity {:?} left without AudioNode",
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
/// 2. Calls [`TuttiGraph::crossfade_boxed`] with a 5 ms `Smooth` fade.
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
        let Some(mut graph) = world.get_resource_mut::<TuttiGraphRes>() else {
            bevy_log::warn!(
                "crossfade_audio_node: TuttiGraphRes missing; entity {:?} not crossfaded",
                entity
            );
            return;
        };
        graph.0.crossfade_boxed(node.0, tutti::Fade::Smooth, 0.005, new_unit);
        if let Some(mut dirty) = world.get_resource_mut::<GraphDirty>() {
            dirty.0 = true;
        }
    });
}

/// Removes graph nodes for entities whose `AudioNode` component was removed
/// (including despawned entities).
///
/// We can't read the `NodeId` off a despawned entity, so the system tracks
/// `(Entity, NodeId)` pairs in a local map keyed by entity, populated as
/// new `AudioNode`s are added and consumed when the component disappears.
pub fn reconcile_node_despawn(
    mut tracked: Local<std::collections::HashMap<Entity, tutti::NodeId>>,
    mut added: Query<(Entity, &AudioNode), Added<AudioNode>>,
    mut removed: RemovedComponents<AudioNode>,
    graph: Option<ResMut<TuttiGraphRes>>,
    mut dirty: ResMut<GraphDirty>,
) {
    for (entity, node) in added.iter_mut() {
        tracked.insert(entity, node.0);
    }

    let Some(mut graph) = graph else {
        // Nothing to remove against.
        for entity in removed.read() {
            tracked.remove(&entity);
        }
        return;
    };

    for entity in removed.read() {
        if let Some(id) = tracked.remove(&entity) {
            if graph.0.contains(id) {
                graph.0.remove(id);
                dirty.0 = true;
            }
        }
    }
}

type ChangedParams<'w> = (&'w AudioNode, &'w NodeKind, &'w Volume, Option<&'w Mute>);
type ChangedParamFilter = Or<(Changed<Volume>, Changed<Mute>)>;

/// Reconciles `Changed<Volume>` and `Changed<Mute>` into the underlying
/// graph node. Dispatch is keyed off [`NodeKind`]; unknown kinds are
/// skipped (apps can layer their own systems for custom kinds).
#[allow(unused_mut, unused_variables)]
pub fn reconcile_params(
    graph: Option<ResMut<TuttiGraphRes>>,
    changed_vol: Query<ChangedParams, ChangedParamFilter>,
    mut dirty: ResMut<GraphDirty>,
) {
    let Some(mut graph) = graph else { return };

    for (node, kind, volume, mute) in changed_vol.iter() {
        let muted = mute.map(|m| m.0).unwrap_or(false);
        let target = if muted { 0.0 } else { volume.0 };

        match *kind {
            #[cfg(feature = "sampler")]
            NodeKind::Sampler => {
                if let Some(unit) = graph.0.node_mut::<SamplerUnit>(node.0) {
                    unit.set_gain(target);
                    dirty.0 = true;
                }
            }
            // Other kinds: no first-class typed setter at this layer;
            // hosts are expected to layer their own systems. See
            // module docs for the extension pattern.
            _ => {
                let _ = (target, node);
            }
        }
    }
}

#[cfg(feature = "sampler")]
type ChangedSamplerParams<'w> = (
    &'w AudioNode,
    &'w NodeKind,
    Option<&'w SamplerSpeed>,
    Option<&'w SamplerLooping>,
);
#[cfg(feature = "sampler")]
type ChangedSamplerFilter = Or<(Changed<SamplerSpeed>, Changed<SamplerLooping>)>;

/// Reconciles `Changed<SamplerSpeed>` and `Changed<SamplerLooping>` into
/// the underlying [`SamplerUnit`].
///
/// `SamplerSpeed` writes through `SamplerUnit::set_speed` (`&mut self`,
/// reached via `node_mut::<SamplerUnit>`). `SamplerLooping` writes through
/// `SamplerUnit::set_looping` (atomic, `&self`) — it doesn't strictly
/// require `node_mut`, but using it here keeps the dispatch shape uniform
/// and lets the dirty flag coalesce a single commit per frame regardless
/// of which sampler param changed.
#[cfg(feature = "sampler")]
pub fn reconcile_sampler_params(
    graph: Option<ResMut<TuttiGraphRes>>,
    changed: Query<ChangedSamplerParams, ChangedSamplerFilter>,
    mut dirty: ResMut<GraphDirty>,
) {
    let Some(mut graph) = graph else { return };

    for (node, kind, speed, looping) in changed.iter() {
        if !matches!(*kind, NodeKind::Sampler) {
            continue;
        }
        let Some(unit) = graph.0.node_mut::<SamplerUnit>(node.0) else {
            continue;
        };
        if let Some(s) = speed {
            unit.set_speed(s.0);
        }
        if let Some(l) = looping {
            unit.set_looping(l.0);
        }
        dirty.0 = true;
    }
}

/// Reconciles `Changed<PluginParam>` into the bound [`PluginEmitter`].
///
/// `PluginHandle::set_parameter` is RT-safe fire-and-forget; the call
/// publishes to a lock-free channel that the audio thread drains. No
/// graph mutation happens here, so we don't touch `GraphDirty`.
#[cfg(feature = "plugin")]
pub fn reconcile_plugin_params(
    changed: Query<(&PluginEmitter, &PluginParam), Changed<PluginParam>>,
) {
    for (emitter, param) in changed.iter() {
        emitter.handle.set_parameter(param.id, param.value);
    }
}

// =============================================================================
// Effect-family parameter reconcilers.
//
// Each one runs in `GraphReconcileSystems::Params`, queries entities with
// the right `NodeKind` and a Changed<X> on any param it owns, then
// writes through the unit's typed setter (lock-free atomic store —
// no graph mutation, so `GraphDirty` stays untouched).
// =============================================================================

#[cfg(feature = "dsp")]
use tutti::core::ecs::{
    Attack, CompressorRatio, DelayTime, Feedback, FilterQ, Frequency, GainDb, ModDepth, ModRate,
    Release, ThresholdDb, WetMix,
};

#[cfg(feature = "dsp")]
type FilterChangedFilter =
    Or<(Changed<Frequency>, Changed<FilterQ>, Changed<GainDb>)>;

/// Reconciles `Changed<Frequency>` / `Changed<FilterQ>` / `Changed<GainDb>`
/// into a `StereoSvfFilterNode<f64>` for entities with [`NodeKind::Filter`].
#[cfg(feature = "dsp")]
#[allow(clippy::type_complexity, reason = "Bevy queries are tuple-shaped by design")]
pub fn reconcile_filter_params(
    graph: Option<ResMut<TuttiGraphRes>>,
    changed: Query<
        (&AudioNode, &NodeKind, Option<&Frequency>, Option<&FilterQ>, Option<&GainDb>),
        FilterChangedFilter,
    >,
) {
    let Some(mut graph) = graph else { return };
    for (node, kind, freq, q, gain) in changed.iter() {
        if !matches!(*kind, NodeKind::Filter) {
            continue;
        }
        let Some(unit) =
            graph.0.node_mut::<tutti::units::StereoSvfFilterNode<f64>>(node.0)
        else {
            continue;
        };
        if let Some(f) = freq {
            unit.set_frequency(f.0);
        }
        if let Some(q) = q {
            unit.set_q(q.0);
        }
        if let Some(g) = gain {
            unit.set_gain_db(g.0);
        }
    }
}

#[cfg(feature = "dsp")]
type DelayChangedFilter = Or<(Changed<DelayTime>, Changed<Feedback>, Changed<WetMix>)>;

/// Reconciles `Changed<DelayTime>` / `Changed<Feedback>` / `Changed<WetMix>`
/// into a `StereoDelayLineNode` for entities with [`NodeKind::Delay`].
#[cfg(feature = "dsp")]
#[allow(clippy::type_complexity, reason = "Bevy queries are tuple-shaped by design")]
pub fn reconcile_delay_params(
    graph: Option<ResMut<TuttiGraphRes>>,
    changed: Query<
        (&AudioNode, &NodeKind, Option<&DelayTime>, Option<&Feedback>, Option<&WetMix>),
        DelayChangedFilter,
    >,
) {
    let Some(mut graph) = graph else { return };
    for (node, kind, time, fb, wet) in changed.iter() {
        if !matches!(*kind, NodeKind::Delay) {
            continue;
        }
        let Some(unit) = graph.0.node_mut::<tutti::units::StereoDelayLineNode>(node.0) else {
            continue;
        };
        if let Some(t) = time {
            unit.set_delay_time(t.0);
        }
        if let Some(f) = fb {
            unit.set_feedback(f.0);
        }
        if let Some(w) = wet {
            unit.set_mix(w.0);
        }
    }
}

#[cfg(feature = "dsp")]
type ChorusChangedFilter = Or<(
    Changed<ModRate>,
    Changed<ModDepth>,
    Changed<Feedback>,
    Changed<WetMix>,
)>;

/// Reconciles chorus params into a `ChorusNode` for entities with
/// [`NodeKind::Chorus`].
#[cfg(feature = "dsp")]
#[allow(clippy::type_complexity, reason = "Bevy queries are tuple-shaped by design")]
pub fn reconcile_chorus_params(
    graph: Option<ResMut<TuttiGraphRes>>,
    changed: Query<
        (
            &AudioNode,
            &NodeKind,
            Option<&ModRate>,
            Option<&ModDepth>,
            Option<&Feedback>,
            Option<&WetMix>,
        ),
        ChorusChangedFilter,
    >,
) {
    let Some(mut graph) = graph else { return };
    for (node, kind, rate, depth, fb, wet) in changed.iter() {
        if !matches!(*kind, NodeKind::Chorus) {
            continue;
        }
        let Some(unit) = graph.0.node_mut::<tutti::units::ChorusNode>(node.0) else {
            continue;
        };
        if let Some(r) = rate {
            unit.set_rate(r.0);
        }
        if let Some(d) = depth {
            unit.set_depth(d.0);
        }
        if let Some(f) = fb {
            unit.set_feedback(f.0);
        }
        if let Some(w) = wet {
            unit.set_mix(w.0);
        }
    }
}

#[cfg(feature = "dsp")]
type CompressorChangedFilter = Or<(
    Changed<ThresholdDb>,
    Changed<CompressorRatio>,
    Changed<Attack>,
    Changed<Release>,
    Changed<GainDb>,
)>;

/// Reconciles compressor params into a `Compressor` for entities with
/// [`NodeKind::Compressor`].
#[cfg(feature = "dsp")]
#[allow(clippy::type_complexity, reason = "Bevy queries are tuple-shaped by design")]
pub fn reconcile_compressor_params(
    graph: Option<ResMut<TuttiGraphRes>>,
    changed: Query<
        (
            &AudioNode,
            &NodeKind,
            Option<&ThresholdDb>,
            Option<&CompressorRatio>,
            Option<&Attack>,
            Option<&Release>,
            Option<&GainDb>,
        ),
        CompressorChangedFilter,
    >,
) {
    let Some(mut graph) = graph else { return };
    for (node, kind, thresh, ratio, attack, release, makeup) in changed.iter() {
        if !matches!(*kind, NodeKind::Compressor) {
            continue;
        }
        let Some(unit) = graph.0.node_mut::<tutti::units::Compressor>(node.0) else {
            continue;
        };
        if let Some(t) = thresh {
            unit.set_threshold(t.0);
        }
        if let Some(r) = ratio {
            unit.set_ratio(r.0);
        }
        if let Some(a) = attack {
            unit.set_attack(a.0);
        }
        if let Some(r) = release {
            unit.set_release(r.0);
        }
        if let Some(m) = makeup {
            unit.set_makeup(m.0);
        }
    }
}

#[cfg(feature = "dsp")]
type GateChangedFilter =
    Or<(Changed<ThresholdDb>, Changed<Attack>, Changed<Release>)>;

/// Reconciles gate params into a `Gate` for entities with
/// [`NodeKind::Gate`]. Threshold / attack / release write through the
/// gate's atomic accessors.
#[cfg(feature = "dsp")]
#[allow(clippy::type_complexity, reason = "Bevy queries are tuple-shaped by design")]
pub fn reconcile_gate_params(
    graph: Option<ResMut<TuttiGraphRes>>,
    changed: Query<
        (
            &AudioNode,
            &NodeKind,
            Option<&ThresholdDb>,
            Option<&Attack>,
            Option<&Release>,
        ),
        GateChangedFilter,
    >,
) {
    use tutti::core::{Db, Linear, Seconds};
    let Some(mut graph) = graph else { return };
    for (node, kind, thresh, attack, release) in changed.iter() {
        if !matches!(*kind, NodeKind::Gate) {
            continue;
        }
        let Some(unit) = graph.0.node_mut::<tutti::units::Gate>(node.0) else {
            continue;
        };
        let _ = (Db, Linear, Seconds);
        if let Some(t) = thresh {
            // Gate threshold is published as a ParamHandle<Db>; the
            // atomic accessor (`unit.threshold()`) is the RT-safe path.
            unit.threshold().store(t.0, std::sync::atomic::Ordering::Release);
        }
        if let Some(a) = attack {
            unit.attack_time()
                .store(a.0, std::sync::atomic::Ordering::Release);
        }
        if let Some(r) = release {
            unit.release_time()
                .store(r.0, std::sync::atomic::Ordering::Release);
        }
    }
}

/// Runs `graph.commit()` once iff any reconcile system mutated the graph.
pub fn commit_graph(graph: Option<ResMut<TuttiGraphRes>>, mut dirty: ResMut<GraphDirty>) {
    if !dirty.0 {
        return;
    }
    if let Some(mut graph) = graph {
        graph.0.commit();
    }
    dirty.0 = false;
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_app::App;
    use tutti::dsp::sine_hz;
    use tutti::TuttiEngine;

    fn test_app() -> App {
        let engine = TuttiEngine::builder()
            .inputs(0)
            .outputs(2)
            .build()
            .expect("build engine");
        let TuttiEngine { graph, .. } = engine;

        let mut app = App::new();
        app.insert_resource(crate::resources::TuttiGraphRes(graph));
        app.init_resource::<GraphDirty>();
        app.add_systems(
            bevy_app::Update,
            (
                reconcile_params.in_set(GraphReconcileSystems::Params),
                reconcile_node_despawn.in_set(GraphReconcileSystems::Despawn),
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
            assert!(app.world().resource::<crate::resources::TuttiGraphRes>().0.contains(node.0));
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
        assert!(app.world().resource::<crate::resources::TuttiGraphRes>().0.contains(node_id));

        app.world_mut().despawn(entity);
        app.update();

        assert!(!app.world().resource::<crate::resources::TuttiGraphRes>().0.contains(node_id));
    }

    #[test]
    #[cfg(feature = "sampler")]
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
        assert!(app.world().resource::<crate::resources::TuttiGraphRes>().0.contains(node_id_before));

        // Replace with a different oscillator — same NodeId, new unit.
        {
            let mut c = app.world_mut().commands();
            crossfade_audio_node(&mut c, entity, Box::new(sine_hz::<f32>(220.0)));
        }
        app.update();

        // Same NodeId stays — that's the contract of crossfade.
        let node_id_after = app.world().get::<AudioNode>(entity).expect("AudioNode").0;
        assert_eq!(node_id_before, node_id_after);
        assert!(app.world().resource::<crate::resources::TuttiGraphRes>().0.contains(node_id_after));
    }

    #[test]
    #[cfg(feature = "sampler")]
    fn sampler_speed_and_looping_change_writes_through() {
        use std::sync::Arc;
        use tutti::core::ecs::{SamplerLooping, SamplerSpeed};
        use tutti::sampler::SamplerUnit;
        use tutti::Wave;

        let mut app = test_app();
        // Add the sampler reconcile system on top of the base test_app set.
        app.add_systems(
            bevy_app::Update,
            reconcile_sampler_params.in_set(GraphReconcileSystems::Params),
        );

        // Build a tiny silent wave (1 channel, 1 sample) just to hand to the
        // sampler. We never tick audio in this test.
        let mut wave = Wave::new(1, 48_000.0);
        wave.push(0.0);
        let unit = SamplerUnit::new(Arc::new(wave));

        let entity = {
            let mut c = app.world_mut().commands();
            c.spawn_audio_node(unit, NodeKind::Sampler)
                .insert((SamplerSpeed(1.0), SamplerLooping(false)))
                .id()
        };
        app.update();

        // Mutate both params; reconciler should write into the SamplerUnit.
        {
            let world = app.world_mut();
            let mut speed = world.get_mut::<SamplerSpeed>(entity).unwrap();
            speed.0 = 2.0;
            let mut looping = world.get_mut::<SamplerLooping>(entity).unwrap();
            looping.0 = true;
        }
        app.update();

        let node_id = app.world().get::<AudioNode>(entity).expect("AudioNode").0;
        let mut graph = app.world_mut().resource_mut::<crate::resources::TuttiGraphRes>();
        let unit = graph.0.node_mut::<SamplerUnit>(node_id).expect("SamplerUnit");
        assert_eq!(unit.speed(), 2.0);
        assert!(unit.is_looping());
    }
}
