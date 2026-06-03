//! Leaf-specific parameter reconcilers.
//!
//! The generic reconcile hub (`SpawnAudioNode`, `GraphReconcileSystems`,
//! `engine_ready`, `GraphDirty`, `crossfade_audio_node`,
//! `reconcile_node_despawn`, `reconcile_params`, `commit_graph`) moved into
//! [`tutti_core::ecs`]. This module keeps only the feature-gated reconcilers
//! that write through leaf-crate unit types (`SamplerUnit`, `PluginEmitter`,
//! the DSP `UnitParam` path, reverb / convolver rebuilds).

#[cfg(any(feature = "dsp", feature = "plugin", feature = "convolution"))]
use bevy_ecs::prelude::*;

#[cfg(feature = "dsp")]
use crate::core::ecs::AudioNode;
#[cfg(feature = "dsp")]
use crate::core::dsp::AudioUnit;

#[cfg(any(feature = "dsp", feature = "convolution"))]
use crate::resources::TuttiGraphRes;

// Generic hub items now live in tutti-core; re-export them here so the leaf
// reconcilers below — and the many external `crate::graph::reconcile::*` paths
// (dsp, automation, spawn, …) — keep resolving against this module.
pub use crate::core::ecs::{
    commit_graph, crossfade_audio_node, engine_ready, reconcile_node_despawn, reconcile_params,
    GraphDirty, GraphReconcileSystems, SpawnAudioNode,
};

#[cfg(feature = "plugin")]
use crate::core::ecs::PluginParam;
#[cfg(feature = "plugin")]
use crate::plugin_host::PluginEmitter;

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
