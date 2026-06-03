//! Bevy `Resource` wrappers around the core engine handles.
//!
//! The shared, leaf-agnostic resources live here in tutti-core: the audio
//! config, the editable DSP graph, the transport handle, and the metering
//! handle. The CPAL driver resource and the feature-subsystem resources
//! (MIDI bus, sampler, soundfont, analysis, plugins, …) stay in bevy-tutti,
//! since they wrap leaf-crate or platform types.
//!
//! `TuttiGraphRes` skips `Deref` so `.0` access keeps the per-frame commit
//! boundary visible.

use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

use crate::TuttiGraph;

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
pub struct TuttiGraphRes(pub TuttiGraph);

/// Lock-free transport handle (play/stop/seek/tempo/loop).
#[derive(Resource, Clone)]
pub struct TransportRes(pub crate::TransportHandle);

impl std::ops::Deref for TransportRes {
    type Target = crate::TransportHandle;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Lock-free metering handle (peak/RMS/LUFS/CPU snapshots).
#[derive(Resource, Clone)]
pub struct MeteringRes(pub crate::MeteringHandle);

impl std::ops::Deref for MeteringRes {
    type Target = crate::MeteringHandle;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
