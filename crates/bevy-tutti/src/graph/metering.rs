//! The master meter's ECS wrapper.
//!
//! The meter is born in [`build_into`](crate::engine::build_into), which shares
//! it with the RT callback, enables it, and inserts it here.
//!
//! # Enabled is a default, not a decision
//!
//! Measuring is on from the start because consumers read
//! [`MasterMeter::get`](tutti_core::metering::MasterMeter::get) through the
//! `Deref` below, and handing four zeros to a host that never opted in is a
//! silent failure. It is reversible — a host that is not watching turns it off
//! the same way:
//!
//! ```rust
//! use bevy_app::prelude::*;
//! use bevy_ecs::prelude::*;
//! use bevy_tutti::prelude::*;
//!
//! fn stop_metering(meter: Res<MeteringRes>) {
//!     meter.disable();
//! }
//!
//! let mut app = App::new();
//! // `build_into` inserts this, already enabled; a device-less app stands the
//! // same resource up by hand.
//! app.insert_resource(MeteringRes::default());
//! app.world().resource::<MeteringRes>().enable();
//! app.add_systems(Update, stop_metering);
//! app.update();
//!
//! assert!(!app.world().resource::<MeteringRes>().is_enabled());
//! ```
//!
//! What that saves is smaller than the switch suggests, which is worth knowing
//! before reaching for it. The stereo *fold* of the device buffer happens in the
//! CPAL callback before `meter_output` is reached, so it runs either way; the
//! switch skips only the deinterleave and the amplitude scan inside it.
//!
//! The analysis tap ([`AudioTapRes`](super::AudioTapRes)) defaults the other
//! way — closed until a host opens it — because its cost is a full buffer copy
//! rather than a scan, and because `open()` returns a consumer somebody has to
//! own.

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
