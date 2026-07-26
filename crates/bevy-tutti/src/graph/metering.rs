//! Metering's Bevy surface: the `MeteringRes` wrapper.
//!
//! Part of the [`crate::ecs`] hub (all of tutti-core's Bevy integration under
//! one roof); the metering value types it wraps stay in [`crate::metering`].
//! Re-exported from `tutti_core::metering` so that path keeps resolving. Only
//! compiled with the `bevy` feature.
//!
//! Construction stays in bevy-tutti's `build_into` (the meter is born
//! mid-sequence and shared with the RT callback), which enables the meter and
//! inserts this wrapper directly — insertion *is* the handoff.

use bevy_ecs::prelude::*;

use crate::metering::MasterMeter;

/// The master output's lock-free peak/RMS meter.
#[derive(Resource, Clone, Default)]
pub struct MeteringRes(pub MasterMeter);

impl std::ops::Deref for MeteringRes {
    type Target = MasterMeter;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

// `TuttiMeteringPlugin` is gone: it existed only to claim `PendingMetering` into
// the wrapper above. `build_into` now calls `meter.enable()` (consumers read
// `MeteringRes::get()` directly, so it has to be measuring) and inserts
// `MeteringRes` directly.
