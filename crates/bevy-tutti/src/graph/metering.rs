//! The master meter's ECS wrapper.
//!
//! The meter is born in [`build_into`](crate::engine::build_into), which shares
//! it with the RT callback, enables it, and inserts it here.
//!
//! # Enabled is a default, not a decision
//!
//! Measuring is on from the start because consumers read
//! [`MasterMeter::get`](tutti_metering::MasterMeter::get) through the
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
//! With the switch off the engine skips the amplitude pass (and the stereo
//! fold of a device that is not stereo) entirely.
//!
//! Copying audio out of the graph is a tap, not the meter: open one with
//! `AudioGraphRes::graph().open_output_tap(..)`, which returns the reader its
//! owner drains.

use bevy_ecs::prelude::*;

use tutti_metering::MasterMeter;

/// The master output's lock-free peak/RMS meter, as a Bevy resource.
///
/// Derefs to [`MasterMeter`]: read it with [`MasterMeter::get`], switch it with
/// `enable`/`disable`. Reading is lock-free and safe from any system; the audio
/// callback writes it.
#[derive(Resource, Clone, Default, Debug)]
pub struct MeteringRes(pub MasterMeter);

impl std::ops::Deref for MeteringRes {
    type Target = MasterMeter;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
