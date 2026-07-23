//! The audio graph: parameter types + (optional) Bevy ECS reconcile hub.
//!
//! **The graph itself is fundsp's [`Net`](crate::dsp::Net)** — there is no
//! tutti wrapper around it. A host wires nodes through `Net`'s imperative API
//! (`add`/`connect`/`disconnect`/`remove`) and calls `commit()` to publish a
//! batch of edits to the audio thread. Everything tutti used to add on top
//! (typed node access, unboxed `add`, output isolation, a readable sample rate)
//! now lives on `Net` itself.
//!
//! The graph handle [`AudioNode`] lives in [`params`] and is always compiled
//! as a plain newtype; under the `bevy` feature it gains `#[derive(Component)]`.
//! The DAW param components (`Volume`/`Pan`/`Mute`/`ModParam`/`PluginParam`)
//! moved out of the engine to `dawai_model::engine_bind::foundational`.
//!
//! The Bevy ECS integration that used to live here now sits in [`crate::ecs`]
//! (the reconcile pipeline, graph resources, param epoch, emitter markers,
//! `GraphReconcilePlugin`, and the transport/metering wrappers). Import those
//! from `tutti_core::ecs::*`.

// Always-compiled node handle. The Bevy `derive` is feature-gated in `params.rs`
// itself, so it degrades to a plain newtype without `ecs`.
pub mod params;

pub use params::AudioNode;
