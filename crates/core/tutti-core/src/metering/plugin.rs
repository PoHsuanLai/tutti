//! Metering's Bevy surface: the `MeteringRes` resource, its engine-claim
//! handoff, the default-enable, and `TuttiMeteringPlugin`.
//!
//! Co-located with the metering subsystem, matching the per-subsystem plugin
//! shape the rest of tutti follows. Only compiled with the `bevy_ecs` feature.
//!
//! Construction stays in bevy-tutti's `build_into` (the meter is born
//! mid-sequence and shared with the RT callback) — this module owns the
//! Bevy-side wrapper, the claim, and metering's own default-enable.

use bevy_app::{App, Plugin};
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

/// Transient handoff: the built master meter. Inserted by `build_into`;
/// claimed into [`MeteringRes`] by [`TuttiMeteringPlugin`]'s `build()`.
#[derive(Resource)]
pub struct PendingMetering(pub Option<MasterMeter>);

/// Bevy plugin: owns metering's Bevy surface. Claims [`PendingMetering`] →
/// [`MeteringRes`] during plugin build, and switches the master meter on —
/// consumers read `MeteringRes::get()` directly, so it has to be measuring.
/// The meter already exists — `build_into` ran synchronously before this
/// plugin was added.
pub struct TuttiMeteringPlugin;

impl Plugin for TuttiMeteringPlugin {
    fn build(&self, app: &mut App) {
        if let Some(PendingMetering(Some(meter))) =
            app.world_mut().remove_resource::<PendingMetering>()
        {
            meter.enable();
            app.insert_resource(MeteringRes(meter));
        }
    }
}
