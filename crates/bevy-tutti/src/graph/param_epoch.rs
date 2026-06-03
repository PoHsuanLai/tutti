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

#![cfg_attr(not(feature = "plugin"), allow(unused_imports))]

use bevy_ecs::prelude::*;

pub use tutti_core::ecs::param_epoch::{bump_param_epoch_core, NodeParamEpoch};

use crate::core::ecs::AudioNode;

#[cfg(feature = "plugin")]
use crate::core::ecs::PluginParam;

// The DSP-family param-epoch bump (`bump_param_epoch_dsp`) moved into
// `tutti_units::ecs::reconcile`. Re-exported via the prelude for compat.

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
