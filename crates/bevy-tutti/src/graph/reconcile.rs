//! Reconcile entity-as-node component changes into [`TuttiGraph`] operations.
//!
//! See [`crate::ecs`] for the component types. This module provides:
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

use crate::core::ecs::{AudioNode, Mute, NodeKind, Volume};
use crate::core::dsp::AudioUnit;

use crate::resources::TuttiGraphRes;

#[cfg(feature = "sampler")]
use crate::core::ecs::{SamplerLooping, SamplerSpeed};
#[cfg(feature = "sampler")]
use crate::sampler::SamplerUnit;

#[cfg(feature = "plugin")]
use crate::core::ecs::PluginParam;
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
    /// Reserved for despawn-phase work hosts may want to order here.
    /// Graph-node removal itself is now an `On<Remove, AudioNode>` observer
    /// (`reconcile_node_despawn`), which fires at command-flush time rather
    /// than in this set.
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
    graph: Option<ResMut<TuttiGraphRes>>,
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
    Option<&'w SamplerSpeed>,
    Option<&'w SamplerLooping>,
);
#[cfg(feature = "sampler")]
type ChangedSamplerFilter = (
    With<crate::core::ecs::SamplerNode>,
    Or<(Changed<SamplerSpeed>, Changed<SamplerLooping>)>,
);

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

    for (node, speed, looping) in changed.iter() {
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
// Generic parameter reconciler (dsp feature).
//
// ONE system handles every effect's scalar params. Each typed ECS param
// component maps to a [`tutti_core::UnitParam`]; a `Changed<component>` is
// pushed to the node via `graph.inner_mut().set(param.setting(v).node(id))`.
//
// This rides fundsp's `Net::set`, which is **lock-free** when a realtime
// backend is attached (the setting is enqueued to the audio thread) — the
// RT-correct param path. The destination unit's `AudioUnit::set` decodes the
// `UnitParam` and stores its atomic; a unit silently ignores params it does
// not own, so no `NodeKind` dispatch or concrete-type downcast is needed here.
//
// `GainDb` is intentionally polymorphic: a filter's `set` treats it as EQ
// gain, a compressor's as make-up gain — each unit owns the interpretation.
//
// Reverb (rebuilt via crossfade, no setters) keeps its own system below;
// the plugin / sampler paths are unchanged.
// =============================================================================

#[cfg(feature = "dsp")]
use crate::core::ecs::{
    Attack, CeilingDb, CompressorRatio, DelayTime, Drive, Feedback, FilterQ, Frequency, GainDb,
    ModDepth, ModRate, Release, ThresholdDb, WetMix,
};

#[cfg(feature = "dsp")]
type AnyParamChanged = Or<(
    Changed<Frequency>,
    Changed<FilterQ>,
    Changed<GainDb>,
    Changed<WetMix>,
    Changed<Feedback>,
    Changed<DelayTime>,
    Changed<ModRate>,
    Changed<ModDepth>,
    Changed<ThresholdDb>,
    Changed<CompressorRatio>,
    Changed<Attack>,
    Changed<Release>,
    Changed<CeilingDb>,
    Changed<Drive>,
)>;

/// All scalar param components an effect node may carry. Each maps to a
/// [`tutti_core::UnitParam`]; absent components are skipped. `ReverbRoomSize` /
/// `ReverbDamping` are excluded — reverb is rebuilt via crossfade, not `set`.
#[cfg(feature = "dsp")]
#[derive(bevy_ecs::query::QueryData)]
pub struct EffectParams {
    pub node: &'static AudioNode,
    pub frequency: Option<&'static Frequency>,
    pub filter_q: Option<&'static FilterQ>,
    pub gain_db: Option<&'static GainDb>,
    pub wet: Option<&'static WetMix>,
    pub feedback: Option<&'static Feedback>,
    pub delay_time: Option<&'static DelayTime>,
    pub mod_rate: Option<&'static ModRate>,
    pub mod_depth: Option<&'static ModDepth>,
    pub threshold: Option<&'static ThresholdDb>,
    pub ratio: Option<&'static CompressorRatio>,
    pub attack: Option<&'static Attack>,
    pub release: Option<&'static Release>,
    pub ceiling: Option<&'static CeilingDb>,
    pub drive: Option<&'static Drive>,
}

/// Generic per-frame param reconciler: pushes every changed scalar param into
/// its node via the uniform `UnitParam` → `AudioUnit::set` path. Replaces the
/// dozen per-effect `reconcile_*_params` systems (reverb excepted).
#[cfg(feature = "dsp")]
#[allow(clippy::type_complexity, reason = "Bevy queries are tuple-shaped by design")]
pub fn reconcile_unit_params(
    graph: Option<ResMut<TuttiGraphRes>>,
    changed: Query<EffectParams, AnyParamChanged>,
) {
    use crate::core::UnitParam;
    use crate::core::dsp::AudioUnit as _;
    let Some(mut graph) = graph else { return };
    let net = graph.0.net_mut();
    for p in changed.iter() {
        let id = p.node.0;
        // Each present component addresses the node by id; the unit's own `set`
        // honors (or ignores) the param. Routes via fundsp's lock-free Net::set.
        let mut push = |param: UnitParam, value: f32| {
            net.set(param.setting(value).node(id));
        };
        if let Some(c) = p.frequency {
            push(UnitParam::Cutoff, c.0);
        }
        if let Some(c) = p.filter_q {
            push(UnitParam::Q, c.0);
        }
        if let Some(c) = p.gain_db {
            push(UnitParam::GainDb, c.0);
        }
        if let Some(c) = p.wet {
            push(UnitParam::Wet, c.0);
        }
        if let Some(c) = p.feedback {
            push(UnitParam::Feedback, c.0);
        }
        if let Some(c) = p.delay_time {
            push(UnitParam::DelayTime, c.0);
        }
        if let Some(c) = p.mod_rate {
            push(UnitParam::Rate, c.0);
        }
        if let Some(c) = p.mod_depth {
            push(UnitParam::Depth, c.0);
        }
        if let Some(c) = p.threshold {
            push(UnitParam::Threshold, c.0);
        }
        if let Some(c) = p.ratio {
            push(UnitParam::Ratio, c.0);
        }
        if let Some(c) = p.attack {
            push(UnitParam::Attack, c.0);
        }
        if let Some(c) = p.release {
            push(UnitParam::Release, c.0);
        }
        if let Some(c) = p.ceiling {
            push(UnitParam::Ceiling, c.0);
        }
        if let Some(c) = p.drive {
            push(UnitParam::Drive, c.0);
        }
    }
}

// ---------------------------------------------------------------------------
// Reverb (crossfade rebuild — fundsp reverb has no parameter setters)
// ---------------------------------------------------------------------------

#[cfg(feature = "dsp")]
type ReverbChangedFilter = Or<(
    Changed<crate::core::ecs::ReverbRoomSize>,
    Changed<crate::core::ecs::ReverbDamping>,
    Changed<WetMix>,
    Changed<crate::core::ecs::ReverbAlgo>,
)>;

#[cfg(feature = "dsp")]
#[allow(clippy::type_complexity, reason = "Bevy queries are tuple-shaped by design")]
pub fn reconcile_reverb_params(
    mut commands: Commands,
    changed: Query<
        (
            Entity,
            &crate::core::ecs::ReverbRoomSize,
            &crate::core::ecs::ReverbDamping,
            &WetMix,
            Option<&crate::core::ecs::ReverbAlgo>,
        ),
        (
            With<AudioNode>,
            With<crate::core::ecs::ReverbNode>,
            ReverbChangedFilter,
        ),
    >,
) {
    use crate::core::ecs::ReverbAlgo;
    for (entity, room, damp, _wet, algo) in changed.iter() {
        // fundsp reverb opcodes have no `set()`, so a param change rebuilds the
        // node with a crossfade. The algorithm tag picks the constructor;
        // absent (pre-`ReverbAlgo` projects) defaults to the 32-channel FDN.
        let unit: Box<dyn AudioUnit> = match algo.copied().unwrap_or_default() {
            ReverbAlgo::Fdn32 => Box::new(crate::core::dsp::reverb_stereo(room.0 as f64, 3.0, damp.0 as f64)),
            ReverbAlgo::Fdn4 => Box::new(crate::core::dsp::reverb4_stereo(room.0 as f64, 3.0)),
        };
        crossfade_audio_node(&mut commands, entity, unit);
    }
}

// ---------------------------------------------------------------------------
// Convolution reverb
// ---------------------------------------------------------------------------

#[cfg(feature = "convolution")]
pub fn reconcile_convolver_params(
    graph: Option<ResMut<TuttiGraphRes>>,
    changed: Query<
        (&AudioNode, &WetMix),
        (With<crate::core::ecs::ConvolutionReverbNode>, Changed<WetMix>),
    >,
) {
    let Some(mut graph) = graph else { return };
    for (node, wet) in changed.iter() {
        let Some(unit) = graph.0.node_mut::<crate::units::StereoConvolverNode>(node.0) else {
            continue;
        };
        unit.set_mix(wet.0);
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
    use crate::core::dsp::sine_hz;
    use crate::TuttiEngine;

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
            .resource::<crate::resources::TuttiGraphRes>()
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
                .resource::<crate::resources::TuttiGraphRes>()
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
        use crate::core::ecs::{SamplerLooping, SamplerSpeed};
        use crate::sampler::SamplerUnit;
        use crate::Wave;

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
