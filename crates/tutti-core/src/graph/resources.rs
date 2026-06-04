//! The audio-graph Bevy resources + the graph's engine-claim handoff.
//!
//! These are the graph-subsystem's own resources: the device config and the
//! editable DSP graph. Transport / metering / midi / sampler / analysis each own
//! their `*Res` next to their subsystem now; this module keeps only what the
//! graph subsystem itself owns.
//!
//! `AudioGraphRes` skips `Deref` so `.0` access keeps the per-frame commit
//! boundary visible.

use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

use crate::AudioGraph;

/// Audio device configuration captured at engine build time.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Reflect)]
#[reflect(Resource, Clone)]
pub struct AudioConfig {
    pub sample_rate: f64,
    pub channels: usize,
}

/// Owns the editable DSP graph. `&mut` edits; call `commit()` once per frame
/// after a batch of edits to publish them to the audio thread.
///
/// Intentionally no `Deref`: graph mutation is paired with the per-frame
/// `commit()` discipline (see `commit_graph`). Keeping access through `.0`
/// makes the dirty/commit boundary visible at the call site.
#[derive(Resource)]
pub struct AudioGraphRes(pub AudioGraph);

/// Transient handoff: the freshly-built graph + its device config.
///
/// `build_into` (bevy-tutti) inserts this after the RT-wiring transaction; the
/// graph subsystem claims it in [`GraphReconcilePlugin`](super::GraphReconcilePlugin)'s
/// `build()` — promoting it into `AudioGraphRes` + `AudioConfig` synchronously,
/// before frame 1 — then drops this transient.
#[derive(Resource)]
pub struct PendingGraph(pub Option<(AudioGraph, AudioConfig)>);
