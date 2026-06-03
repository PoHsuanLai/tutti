//! Leaf-specific parameter-epoch bumps.
//!
//! The generic [`NodeParamEpoch`] resource + [`bump_param_epoch_core`] moved
//! into [`tutti_core::ecs::param_epoch`]; they are re-exported here so existing
//! `crate::graph::param_epoch::{NodeParamEpoch, bump_param_epoch_core}` paths
//! hold. The sampler / plugin / dsp-family bumps stay here because they
//! reference feature-gated leaf param components, and `cfg` can't be applied to
//! elements of an `Or<>` tuple — each feature gets its own self-contained
//! system.
//!
//! See the core module for the design rationale.

#![cfg_attr(
    not(any(feature = "sampler", feature = "plugin", feature = "dsp")),
    allow(unused_imports)
)]

use bevy_ecs::prelude::*;

pub use tutti_core::ecs::param_epoch::{bump_param_epoch_core, NodeParamEpoch};

use crate::core::ecs::AudioNode;

#[cfg(feature = "dsp")]
use crate::core::ecs::{
    Attack, CeilingDb, CompressorRatio, DelayTime, Drive, Feedback, FilterQ, Frequency, GainDb,
    ModDepth, ModRate, Release, ThresholdDb, WetMix,
};

#[cfg(feature = "sampler")]
use crate::core::ecs::{SamplerLooping, SamplerSpeed};

#[cfg(feature = "plugin")]
use crate::core::ecs::PluginParam;

#[cfg(feature = "sampler")]
type SamplerParamChanged = Or<(Changed<SamplerSpeed>, Changed<SamplerLooping>)>;

/// Bump the epoch for sampler param changes (`SamplerSpeed`, `SamplerLooping`).
#[cfg(feature = "sampler")]
pub fn bump_param_epoch_sampler(
    mut epoch: ResMut<NodeParamEpoch>,
    changed: Query<&AudioNode, SamplerParamChanged>,
) {
    for node in changed.iter() {
        epoch.bump(node.0);
    }
}

/// Bump the epoch for plugin param changes (`PluginParam`).
#[cfg(feature = "plugin")]
pub fn bump_param_epoch_plugin(
    mut epoch: ResMut<NodeParamEpoch>,
    changed: Query<&AudioNode, Changed<PluginParam>>,
) {
    for node in changed.iter() {
        epoch.bump(node.0);
    }
}

/// DSP-family param components (filter / delay / chorus / dynamics / …). All
/// gated behind the `dsp` feature, mirroring the reconcilers that write them.
#[cfg(feature = "dsp")]
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
#[cfg(feature = "dsp")]
pub fn bump_param_epoch_dsp(
    mut epoch: ResMut<NodeParamEpoch>,
    changed: Query<&AudioNode, DspParamChanged>,
) {
    for node in changed.iter() {
        epoch.bump(node.0);
    }
}
