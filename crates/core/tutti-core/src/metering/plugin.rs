//! Metering's Bevy surface: the `MeteringRes` resource, its engine-claim
//! handoff, the default-enable, and `TuttiMeteringPlugin`.
//!
//! Co-located with the metering subsystem, matching the per-subsystem plugin
//! shape the rest of tutti follows. Only compiled with the `bevy_ecs` feature.
//!
//! Construction stays in bevy-tutti's `build_into` (the metering manager Arc is
//! born mid-sequence and shared with the RT callback) — this module owns the
//! Bevy-side wrapper, the claim, and metering's own default-enable (amp + CPU),
//! which used to live inline in `build_into`.

use bevy_app::{App, Plugin};
use bevy_ecs::prelude::*;

use crate::MeteringHandle;

/// Lock-free metering handle (peak/RMS/LUFS/CPU snapshots).
#[derive(Resource, Clone)]
pub struct MeteringRes(pub MeteringHandle);

impl std::ops::Deref for MeteringRes {
    type Target = MeteringHandle;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Transient handoff: the built metering handle. Inserted by `build_into`;
/// claimed into [`MeteringRes`] by [`TuttiMeteringPlugin`]'s `build()`.
#[derive(Resource)]
pub struct PendingMetering(pub Option<MeteringHandle>);

/// Bevy plugin: owns metering's Bevy surface. Claims [`PendingMetering`] →
/// [`MeteringRes`] during plugin build, and enables metering's own defaults
/// (amplitude + CPU). The handle already exists — `build_into` ran synchronously
/// before this plugin was added.
pub struct TuttiMeteringPlugin;

impl Plugin for TuttiMeteringPlugin {
    fn build(&self, app: &mut App) {
        if let Some(PendingMetering(Some(handle))) =
            app.world_mut().remove_resource::<PendingMetering>()
        {
            // Metering owns its own defaults: amplitude + CPU on by default,
            // since consumers read `MeteringRes::amplitude()` / `cpu_*()` directly.
            handle.inner().enable_amp();
            handle.inner().cpu().enable();
            app.insert_resource(MeteringRes(handle));
        }
    }
}
