//! Leaf-specific parameter reconcilers.
//!
//! The generic reconcile hub (`SpawnAudioNode`, `GraphReconcileSystems`,
//! `engine_ready`, `GraphDirty`, `crossfade_audio_node`,
//! `reconcile_node_despawn`, `reconcile_params`, `commit_graph`) moved into
//! [`tutti_core::ecs`]. This module keeps only the feature-gated reconcilers
//! that write through leaf-crate unit types (`SamplerUnit`, `PluginEmitter`,
//! the DSP `UnitParam` path, reverb / convolver rebuilds).

use bevy_ecs::prelude::*;

use crate::core::ecs::AudioNode;
use crate::core::dsp::AudioUnit;

use crate::resources::TuttiGraphRes;

// Generic hub items now live in tutti-core; re-export them here so the leaf
// reconcilers below — and the many external `crate::graph::reconcile::*` paths
// (dsp, automation, spawn, …) — keep resolving against this module.
pub use crate::core::ecs::{
    commit_graph, crossfade_audio_node, engine_ready, reconcile_node_despawn, reconcile_params,
    GraphDirty, GraphReconcileSystems, SpawnAudioNode,
};

#[cfg(feature = "sampler")]
use crate::core::ecs::{NodeKind, SamplerLooping, SamplerSpeed, Volume};
#[cfg(feature = "sampler")]
use crate::sampler::SamplerUnit;

#[cfg(feature = "plugin")]
use crate::core::ecs::PluginParam;
#[cfg(feature = "plugin")]
use crate::plugin_host::PluginEmitter;

/// Sampler volume write-through.
///
/// Mirrors the generic `reconcile_params` skeleton in tutti-core, layering the
/// `NodeKind::Sampler` arm that core deliberately omits (it can't reference
/// `SamplerUnit`). A `Changed<Volume>`/`Changed<Mute>` on a sampler entity sets
/// the unit gain and marks the graph dirty.
#[cfg(feature = "sampler")]
type ChangedSamplerVolume<'w> = (&'w AudioNode, &'w NodeKind, &'w Volume, Option<&'w crate::core::ecs::Mute>);
#[cfg(feature = "sampler")]
type ChangedSamplerVolumeFilter = Or<(Changed<Volume>, Changed<crate::core::ecs::Mute>)>;

#[cfg(feature = "sampler")]
pub fn reconcile_sampler_volume(
    mut graph: ResMut<TuttiGraphRes>,
    changed: Query<ChangedSamplerVolume, ChangedSamplerVolumeFilter>,
    mut dirty: ResMut<GraphDirty>,
) {
    for (node, kind, volume, mute) in changed.iter() {
        if *kind != NodeKind::Sampler {
            continue;
        }
        let muted = mute.map(|m| m.0).unwrap_or(false);
        let target = if muted { 0.0 } else { volume.0 };
        if let Some(unit) = graph.0.node_mut::<SamplerUnit>(node.0) {
            unit.set_gain(target);
            dirty.0 = true;
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
    mut graph: ResMut<TuttiGraphRes>,
    changed: Query<ChangedSamplerParams, ChangedSamplerFilter>,
    mut dirty: ResMut<GraphDirty>,
) {
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
    mut graph: ResMut<TuttiGraphRes>,
    changed: Query<EffectParams, AnyParamChanged>,
) {
    use crate::core::UnitParam;
    use crate::core::dsp::AudioUnit as _;
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
type ChangedConvolverParams<'w> = (&'w AudioNode, &'w WetMix);
#[cfg(feature = "convolution")]
type ChangedConvolverFilter = (With<crate::core::ecs::ConvolutionReverbNode>, Changed<WetMix>);

#[cfg(feature = "convolution")]
pub fn reconcile_convolver_params(
    mut graph: ResMut<TuttiGraphRes>,
    changed: Query<ChangedConvolverParams, ChangedConvolverFilter>,
) {
    for (node, wet) in changed.iter() {
        let Some(unit) = graph.0.node_mut::<crate::units::StereoConvolverNode>(node.0) else {
            continue;
        };
        unit.set_mix(wet.0);
    }
}

#[cfg(test)]
#[cfg(feature = "sampler")]
mod tests {
    use super::*;
    use crate::core::dsp::sine_hz;
    use crate::core::ecs::NodeKind;
    use crate::TuttiEngine;
    use bevy_app::App;

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
    fn sampler_speed_and_looping_change_writes_through() {
        use std::sync::Arc;
        use crate::core::ecs::{SamplerLooping, SamplerNode, SamplerSpeed};
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
            // `spawn_audio_node` attaches only `AudioNode` + `NodeKind`; the
            // `SamplerNode` authoring marker must be inserted alongside, exactly
            // as every production sampler spawn path does (e.g.
            // `promote_pending_samplers`). `reconcile_sampler_params` filters on
            // `With<SamplerNode>`, so without it the reconcile is skipped.
            c.spawn_audio_node(unit, NodeKind::Sampler)
                .insert((SamplerNode, SamplerSpeed(1.0), SamplerLooping(false)))
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
