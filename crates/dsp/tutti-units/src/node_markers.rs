//! DSP node authoring markers — one ZST per known-param node type.
//!
//! Each `#[require(...)]`s the param components it needs (all carry sensible
//! `Default`s, see [`crate::params`]). A marker *is* the node-type identity: a
//! type-specific reconciler filters on `With<ThatMarker>` rather than matching a
//! central dispatch enum, so the marker on the entity is the single source of
//! truth for "what kind of node is this".
//!
//! `AudioNode` is deliberately NOT in any `#[require]` list: it wraps a foreign
//! non-`Reflect` `NodeId` and is inserted by the spawn system *after*
//! `graph.add(unit)`. Marker (authored, Reflect) + AudioNode (runtime handle)
//! coexist on the same entity.
//!
//! These live in tutti-units (next to the spawn/reconcile systems that drive
//! them); the sampler/foundational params stay in tutti-core. Kept in this
//! dedicated module rather than glob-exported at the crate root so the
//! `ChorusNode` *marker* doesn't collide with the `ChorusNode` *DSP unit*.

use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

use crate::dsp_params::{
    Attack, BeatSynced, CompressorRatio, DelayTime, Feedback, FilterQ, Frequency, GainDb, ModDepth,
    ModRate, Release, ReverbAlgo, ReverbDamping, ReverbRoomSize, ThresholdDb, WetMix,
};

/// Authoring marker for a dynamics compressor node.
#[derive(Component, Reflect, Default, Clone, Copy, Debug)]
#[reflect(Component, Default)]
#[require(ThresholdDb, CompressorRatio, Attack, Release, GainDb)]
pub struct CompressorNode;
/// Authoring marker for a noise gate node.
#[derive(Component, Reflect, Default, Clone, Copy, Debug)]
#[reflect(Component, Default)]
#[require(ThresholdDb, Attack, Release)]
pub struct GateNode;
/// Authoring marker for a state-variable filter node.
#[derive(Component, Reflect, Default, Clone, Copy, Debug)]
#[reflect(Component, Default)]
#[require(Frequency, FilterQ, GainDb)]
pub struct FilterNode;
/// Authoring marker for a stereo reverb node.
#[derive(Component, Reflect, Default, Clone, Copy, Debug)]
#[reflect(Component, Default)]
#[require(ReverbRoomSize, ReverbDamping, WetMix, ReverbAlgo)]
pub struct ReverbNode;
/// Authoring marker for an FFT convolution reverb node.
#[derive(Component, Reflect, Default, Clone, Copy, Debug)]
#[reflect(Component, Default)]
#[require(WetMix)]
pub struct ConvolutionReverbNode;
/// Authoring marker for a stereo delay node.
#[derive(Component, Reflect, Default, Clone, Copy, Debug)]
#[reflect(Component, Default)]
#[require(DelayTime, Feedback, WetMix)]
pub struct DelayNode;
/// Authoring marker for a stereo chorus node.
#[derive(Component, Reflect, Default, Clone, Copy, Debug)]
#[reflect(Component, Default)]
#[require(ModRate, ModDepth, Feedback, WetMix)]
pub struct ChorusNode;
/// Authoring marker for an LFO modulator node.
///
/// Its required params are `Frequency` + `ModDepth`, with `LfoShapeKind`
/// + `BeatSynced` as construction-only authored data.
#[derive(Component, Reflect, Default, Clone, Copy, Debug)]
#[reflect(Component, Default)]
#[require(Frequency, ModDepth, crate::dsp_params::LfoShapeKind, BeatSynced)]
pub struct LfoNodeMarker;
/// Register the DSP authoring markers for reflection. Called by `TuttiDspPlugin`.
/// Idempotent — Bevy ignores duplicate `register_type`.
pub fn register_node_markers(app: &mut bevy_app::App) {
    app.register_type::<CompressorNode>()
        .register_type::<GateNode>()
        .register_type::<FilterNode>()
        .register_type::<ReverbNode>()
        .register_type::<ConvolutionReverbNode>()
        .register_type::<DelayNode>()
        .register_type::<ChorusNode>()
        .register_type::<LfoNodeMarker>();
}
