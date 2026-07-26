//! The master meter's ECS wrapper.
//!
//! The meter is born in [`build_into`](crate::engine::build_into), which shares
//! it with the RT callback, enables it (consumers read [`MeteringRes::get`]
//! directly, so it has to be measuring), and inserts it here.

use bevy_ecs::prelude::*;

use tutti_core::metering::MasterMeter;

/// The master output's lock-free peak/RMS meter.
#[derive(Resource, Clone, Default)]
pub struct MeteringRes(pub MasterMeter);

impl std::ops::Deref for MeteringRes {
    type Target = MasterMeter;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
