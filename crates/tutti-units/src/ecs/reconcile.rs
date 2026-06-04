//! Unit-specific parameter reconcilers + DSP param-epoch bump.
//!
//! The generic reconcile hub (`SpawnAudioNode`, `GraphReconcileSystems`,
//! `engine_ready`, `GraphDirty`, `reconcile_node_despawn`, `reconcile_params`,
//! `commit_graph`, the `NodeParamEpoch` resource + `bump_param_epoch_core`)
//! lives in [`tutti_core::ecs`]. This module keeps the reconcilers that write
//! through this crate's unit types (the DSP `UnitParam` path, reverb /
//! convolver rebuilds), plus the DSP-family param-epoch bump.

use bevy_ecs::prelude::*;

use tutti_core::dsp::AudioUnit;
use tutti_core::ecs::{AudioNode, NodeParamEpoch, TuttiGraphRes};

use tutti_core::ecs::GraphReconcileSystems;

// =============================================================================
// Generic parameter reconciler.
//
// ONE system handles every effect's scalar params. Each typed ECS param
// component maps to a [`tutti_core::UnitParam`]; a `Changed<component>` is
// pushed to the node via `net.set(param.setting(v).node(id))`.
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
// the plugin / sampler paths are handled by their own leaf crates.
// =============================================================================

use crate::dsp_params::{
    Attack, CeilingDb, CompressorRatio, DelayTime, Drive, Feedback, FilterQ, Frequency, GainDb,
    ModDepth, ModRate, Release, ThresholdDb, WetMix,
};

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
#[allow(clippy::type_complexity, reason = "Bevy queries are tuple-shaped by design")]
pub fn reconcile_unit_params(
    mut graph: ResMut<TuttiGraphRes>,
    changed: Query<EffectParams, AnyParamChanged>,
) {
    use tutti_core::dsp::AudioUnit as _;
    use tutti_core::UnitParam;
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

type ReverbChangedFilter = Or<(
    Changed<crate::dsp_params::ReverbRoomSize>,
    Changed<crate::dsp_params::ReverbDamping>,
    Changed<WetMix>,
    Changed<crate::dsp_params::ReverbAlgo>,
)>;

#[allow(clippy::type_complexity, reason = "Bevy queries are tuple-shaped by design")]
pub fn reconcile_reverb_params(
    mut commands: Commands,
    changed: Query<
        (
            Entity,
            &crate::dsp_params::ReverbRoomSize,
            &crate::dsp_params::ReverbDamping,
            &WetMix,
            Option<&crate::dsp_params::ReverbAlgo>,
        ),
        (
            With<AudioNode>,
            With<crate::node_markers::ReverbNode>,
            ReverbChangedFilter,
        ),
    >,
) {
    use tutti_core::ecs::crossfade_audio_node;
    use crate::dsp_params::ReverbAlgo;
    for (entity, room, damp, _wet, algo) in changed.iter() {
        // fundsp reverb opcodes have no `set()`, so a param change rebuilds the
        // node with a crossfade. The algorithm tag picks the constructor;
        // absent (pre-`ReverbAlgo` projects) defaults to the 32-channel FDN.
        let unit: Box<dyn AudioUnit> = match algo.copied().unwrap_or_default() {
            ReverbAlgo::Fdn32 => Box::new(tutti_core::dsp::reverb_stereo(room.0 as f64, 3.0, damp.0 as f64)),
            ReverbAlgo::Fdn4 => Box::new(tutti_core::dsp::reverb4_stereo(room.0 as f64, 3.0)),
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
type ChangedConvolverFilter = (With<crate::node_markers::ConvolutionReverbNode>, Changed<WetMix>);

#[cfg(feature = "convolution")]
pub fn reconcile_convolver_params(
    mut graph: ResMut<TuttiGraphRes>,
    changed: Query<ChangedConvolverParams, ChangedConvolverFilter>,
) {
    for (node, wet) in changed.iter() {
        let Some(unit) = graph.0.node_mut::<crate::StereoConvolverNode>(node.0) else {
            continue;
        };
        unit.set_mix(wet.0);
    }
}

// ---------------------------------------------------------------------------
// DSP-family param-epoch bump.
//
// Pure change-detection bump: reads `Changed<T>`, never the graph, so it stays
// ungated (mirrors the ungated core epoch bump).
// ---------------------------------------------------------------------------

type DspParamChanged = Or<(
    Changed<Frequency>,
    Changed<FilterQ>,
    Changed<GainDb>,
    Changed<DelayTime>,
    Changed<Feedback>,
    Changed<WetMix>,
    Changed<ModRate>,
    Changed<ModDepth>,
    Changed<ThresholdDb>,
    Changed<CompressorRatio>,
    Changed<Attack>,
    Changed<Release>,
    Changed<Drive>,
    Changed<CeilingDb>,
)>;

/// Bump the epoch for every node whose DSP-family param component changed.
pub fn bump_param_epoch_dsp(
    mut epoch: ResMut<NodeParamEpoch>,
    changed: Query<&AudioNode, DspParamChanged>,
) {
    for node in changed.iter() {
        epoch.bump(node.0);
    }
}

/// Schedule the DSP param reconcilers + epoch bump.
///
/// Called by [`super::TuttiDspPlugin`]. The graph-touching reconcilers run in
/// the `Params` set gated on `engine_ready`; the epoch bump stays ungated.
pub(super) fn build(app: &mut bevy_app::App) {
    use tutti_core::ecs::engine_ready;
    app.add_systems(
        bevy_app::Update,
        (
            reconcile_unit_params.in_set(GraphReconcileSystems::Params),
            reconcile_reverb_params.in_set(GraphReconcileSystems::Params),
        )
            .run_if(engine_ready),
    );
    app.add_systems(bevy_app::Update, bump_param_epoch_dsp);
}
