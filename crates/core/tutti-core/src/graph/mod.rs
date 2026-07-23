//! The audio graph: parameter types + (optional) Bevy ECS reconcile hub.
//!
//! **The graph itself is fundsp's [`Net`](crate::dsp::Net)** — there is no
//! tutti wrapper around it. A host wires nodes through `Net`'s imperative API
//! (`add`/`connect`/`disconnect`/`remove`) and calls `commit()` to publish a
//! batch of edits to the audio thread. Everything tutti used to add on top
//! (typed node access, unboxed `add`, output isolation, a readable sample rate)
//! now lives on `Net` itself.
//!
//! The parameter *types* ([`AudioNode`], [`Volume`], [`Pan`],
//! [`Mute`], [`PluginParam`], [`ModParam`], [`LayerKey`]) live in [`params`]
//! and are always compiled as plain structs; under the `bevy` feature they
//! gain `#[derive(Component, Reflect)]`.
//!
//! The Bevy ECS integration that used to live here now sits in [`crate::ecs`]
//! (the reconcile pipeline, graph resources, param epoch, emitter markers,
//! `GraphReconcilePlugin`, and the transport/metering wrappers). Import those
//! from `tutti_core::ecs::*`. This module keeps only the always-compiled
//! parameter *types*, which stay plain data a non-Bevy host can carry.

// Always-compiled parameter types. The Bevy `derive`s on these are feature-gated
// in `params.rs` itself, so they degrade to plain structs without `ecs`.
pub mod params;

pub use params::{AudioNode, LayerKey, ModParam, Mute, Pan, PluginParam, Volume};
