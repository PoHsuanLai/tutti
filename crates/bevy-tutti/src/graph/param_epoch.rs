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

pub use tutti_core::ecs::param_epoch::{bump_param_epoch_core, NodeParamEpoch};

// The DSP-family param-epoch bump (`bump_param_epoch_dsp`) moved into
// `tutti_units::ecs::reconcile`; the plugin bump (`bump_param_epoch_plugin`)
// moved into the `tutti-plugin-host` crate. Re-exported via the prelude.
