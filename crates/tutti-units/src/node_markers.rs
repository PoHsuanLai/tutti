//! DSP node authoring markers — one ZST per known-param `NodeKind`.
//!
//! Each `#[require(...)]`s the param components it needs (all carry sensible
//! `Default`s, see [`crate::params`]) and exposes a `KIND` const so the spawn
//! system can insert `(Marker, Marker::KIND)` together, keeping the authored
//! marker and the by-value `NodeKind` dispatch tag in lockstep.
//!
//! `AudioNode` is deliberately NOT in any `#[require]` list: it wraps a foreign
//! non-`Reflect` `NodeId` and is inserted by the spawn system *after*
//! `graph.add(unit)`. Marker (authored, Reflect) + AudioNode (runtime handle)
//! coexist on the same entity.
//!
//! These live in tutti-units (next to the spawn/reconcile systems that drive
//! them); `NodeKind` and the sampler/foundational params stay in tutti-core.
//! Kept in this dedicated module rather than glob-exported at the crate root so
//! the `ChorusNode` *marker* doesn't collide with the `ChorusNode` *DSP unit*.

use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

use tutti_core::graph::NodeKind;

use crate::dsp_params::{
    Attack, BeatSynced, CompressorRatio, DelayTime, Feedback, Frequency, FilterQ, GainDb, ModDepth,
    ModRate, Release, ReverbAlgo, ReverbDamping, ReverbRoomSize, ThresholdDb, WetMix,
};

/// Authoring marker for a dynamics compressor node.
#[derive(Component, Reflect, Default, Clone, Copy, Debug)]
#[reflect(Component, Default)]
#[require(ThresholdDb, CompressorRatio, Attack, Release, GainDb)]
pub struct CompressorNode;
impl CompressorNode {
    pub const KIND: NodeKind = NodeKind::Compressor;
}

/// Authoring marker for a noise gate node.
#[derive(Component, Reflect, Default, Clone, Copy, Debug)]
#[reflect(Component, Default)]
#[require(ThresholdDb, Attack, Release)]
pub struct GateNode;
impl GateNode {
    pub const KIND: NodeKind = NodeKind::Gate;
}

/// Authoring marker for a state-variable filter node.
#[derive(Component, Reflect, Default, Clone, Copy, Debug)]
#[reflect(Component, Default)]
#[require(Frequency, FilterQ, GainDb)]
pub struct FilterNode;
impl FilterNode {
    pub const KIND: NodeKind = NodeKind::Filter;
}

/// Authoring marker for a stereo reverb node.
#[derive(Component, Reflect, Default, Clone, Copy, Debug)]
#[reflect(Component, Default)]
#[require(ReverbRoomSize, ReverbDamping, WetMix, ReverbAlgo)]
pub struct ReverbNode;
impl ReverbNode {
    pub const KIND: NodeKind = NodeKind::Reverb;
}

/// Authoring marker for an FFT convolution reverb node.
#[derive(Component, Reflect, Default, Clone, Copy, Debug)]
#[reflect(Component, Default)]
#[require(WetMix)]
pub struct ConvolutionReverbNode;
impl ConvolutionReverbNode {
    pub const KIND: NodeKind = NodeKind::ConvolutionReverb;
}

/// Authoring marker for a stereo delay node.
#[derive(Component, Reflect, Default, Clone, Copy, Debug)]
#[reflect(Component, Default)]
#[require(DelayTime, Feedback, WetMix)]
pub struct DelayNode;
impl DelayNode {
    pub const KIND: NodeKind = NodeKind::Delay;
}

/// Authoring marker for a stereo chorus node.
#[derive(Component, Reflect, Default, Clone, Copy, Debug)]
#[reflect(Component, Default)]
#[require(ModRate, ModDepth, Feedback, WetMix)]
pub struct ChorusNode;
impl ChorusNode {
    pub const KIND: NodeKind = NodeKind::Chorus;
}

/// Authoring marker for an LFO modulator node.
///
/// Its required params are `Frequency` + `ModDepth`, with `LfoShapeKind`
/// + `BeatSynced` as construction-only authored data.
#[derive(Component, Reflect, Default, Clone, Copy, Debug)]
#[reflect(Component, Default)]
#[require(Frequency, ModDepth, crate::dsp_params::LfoShapeKind, BeatSynced)]
pub struct LfoNodeMarker;
impl LfoNodeMarker {
    pub const KIND: NodeKind = NodeKind::Lfo;
}

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
