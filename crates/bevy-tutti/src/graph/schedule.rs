//! When reconcile work runs, and whether it runs at all.
//!
//! The ordering anchor ([`GraphReconcileSystems`]), the run-condition every
//! engine-touching system is gated on ([`engine_ready`]), and the per-frame
//! coalescing flag ([`GraphDirty`]) that decides whether the frame ends in a
//! commit.
//!
//! None of this has an engine counterpart, and that is the point: the graph has no
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
/// # What this does **not** guarantee
///
/// This reads a *state*, not the resources. `AudioEngineState::Running` is a
/// value any host can insert, and the crate's own tests and examples do exactly
/// that to exercise graph systems without a device. So a system gated here may
/// still run in a `World` where `build_into` never executed, and **every
/// resource `build_into` inserts can be absent**: `MetronomeRes`, `MeteringRes`,
/// `AudioTapRes`, `MidiBusRes`, `MidiRoutingRes`, `ClockMasterRes`,
/// `DiskStreamerRes`, `MidiIoRes`, and the compensation cell. Add `PluginsRes`,
/// which is inserted lazily and not by the engine block at all.
///
/// In Bevy 0.19 a missing `Res<T>` is a **parameter-validation failure that
/// panics the schedule**, not a skipped system — so a hard `Res` on any of the
/// above turns "this host did not build an engine" into a crash. Take them as
/// `Option<Res<_>>` and return early.
///
/// The safe ones are those inserted by the *same plugin* that schedules the
/// system, since a host cannot have one without the other. That is the real
/// test — not whether the engine is up, but who owns the insertion. Treating the
/// list above as short enough to take hard is how a host that adds
/// `TuttiMidiPlugin` without the full engine bootstrap gets a panic instead of a
/// no-op.
pub fn engine_ready(state: Option<Res<AudioEngineState>>) -> bool {
    state.is_some_and(|s| s.is_running())
}

/// Per-frame "did anything change?" flag that coalesces `graph.commit()` to at
/// most one call per frame.
///
/// Any reconcile system that mutates the graph sets it; [`commit_graph`] clears
/// it. Set it after staging an edit rather than committing inline — a commit per
/// edit costs a graph rebuild per edit.
///
/// [`commit_graph`]: super::commit_graph
#[derive(Resource, Default)]
pub struct GraphDirty(
    /// Whether any reconcile system mutated the graph this frame.
    pub bool,
);
