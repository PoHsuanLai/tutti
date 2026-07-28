//! When reconcile work runs, and whether it runs at all.
//!
//! The ordering anchor ([`GraphReconcileSystems`]), the run-condition every
//! engine-touching system is gated on ([`engine_ready`]), and the per-frame
//! coalescing flag ([`GraphDirty`]) that decides whether the frame ends in a
//! commit.
//!
//! None of this has an engine counterpart, and that is the point: `Net` has no
//! dirty bit and no notion of a frame. Batching a frame's edits into one
//! `commit()` is the ECS binding's own duty, which is why this state lives here
//! and dies at the end of the frame.

use bevy_ecs::prelude::*;
use bevy_ecs::schedule::SystemSet;

use crate::AudioEngineState;

/// System-set ordering anchor for the reconcile pipeline.
///
/// Apps can schedule their own systems against these sets. The plugin
/// runs them in the order: `Spawn` → `Params` → `Despawn` → `Compensate` →
/// `Commit`, all inside `Update`.
#[derive(SystemSet, Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum GraphReconcileSystems {
    /// Initial spawn of new graph nodes (rare; mostly app-driven via
    /// [`SpawnAudioNode`](super::SpawnAudioNode)). Apps can hook here to
    /// populate parameter components on the same frame the node is created.
    Spawn,
    /// Parameter-component changes are written into the graph here.
    Params,
    /// Reserved for despawn-phase work hosts may want to order here.
    /// Graph-node removal itself is an `On<Remove, AudioNode>` observer
    /// ([`reconcile_node_despawn`](super::reconcile_node_despawn)), which fires
    /// at command-flush time rather than in this set.
    Despawn,
    /// Latency compensation, if the app opted into it.
    ///
    /// **Empty by default** — nothing in this crate runs here. It sits between
    /// `Despawn` and `Commit` because compensation must see the frame's final
    /// topology, yet must reach the audio thread in the same commit. Apps that
    /// want delay compensation add a system here; `bevy_tutti` ships
    /// [`LatencyCompensationPlugin`](crate::LatencyCompensationPlugin) for
    /// exactly that.
    Compensate,
    /// Single `graph.commit()` if any earlier set mutated the graph.
    Commit,
}

/// Run-condition: the audio engine is running.
///
/// Reads [`AudioEngineState`], which `build_into` sets to
/// [`Running`](AudioEngineState::Running) only after every engine resource
/// (`AudioGraphRes`, `TransportRes`, `MeteringRes`, `AudioConfig`, and the
/// feature subsystem resources) has been inserted. That happens synchronously
/// during plugin build, before frame 1, so a system gated on this can take its
/// engine resources as plain `Res`/`ResMut` rather than `Option<Res<_>>` plus a
/// `let Some(..) else` guard — it simply does not run when there is no engine
/// (the idiomatic Bevy shape, mirroring `bevy_audio`'s
/// `audio_output_available`).
///
/// The state is the source of truth rather than `resource_exists::<AudioGraphRes>`
/// so that "is the engine up?" has one answer. `Option<Res<_>>` here also keeps
/// the condition usable in a `World` that never added
/// [`TuttiPlugin`](crate::TuttiPlugin) — a missing state reads as not-ready.
///
/// Two resources are *not* covered (they may be absent even when the engine
/// built) and must keep `Option<Res<_>>`: `MidiIoRes` (only when a hardware
/// MIDI port opened) and `PluginsRes` (inserted lazily, not in the engine
/// block).
pub fn engine_ready(state: Option<Res<AudioEngineState>>) -> bool {
    state.is_some_and(|s| s.is_running())
}

/// Per-frame "did anything change?" flag used to coalesce
/// `graph.commit()` to at most one call per frame.
#[derive(Resource, Default)]
pub struct GraphDirty(pub bool);
