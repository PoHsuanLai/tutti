//! The master meter's ECS wrapper.
//!
//! The meter is declared by the session on the graph's outputs (an op in the
//! compiled plan, so a device callback and an offline render both feed it),
//! and [`build_into`](crate::engine::build_into) hands its reader to this
//! resource.
//!
//! # Enabled is a default, not a decision
//!
//! Measuring is on from the start because consumers read
//! [`MeterReader::read`](tutti_graph::MeterReader::read) through the `Deref`
//! below, and handing zeros to a host that never opted in is a silent failure.
//! It is reversible: a host that is not watching turns it off the same way.
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
//! // same resource up from a session's reader by hand.
//! let mut session = tutti_session::Session::builder()
//!     .build_detached(tutti_types::SampleRate(48_000.0))
//!     .expect("builds");
//! app.insert_resource(MeteringRes(session.take_master_meter().expect("declared")));
//! assert!(app.world().resource::<MeteringRes>().is_enabled());
//! app.add_systems(Update, stop_metering);
//! app.update();
//!
//! assert!(!app.world().resource::<MeteringRes>().is_enabled());
//! ```
//!
//! With the switch off the op skips its pass over the outputs entirely.
//!
//! # One consumer
//!
//! A read takes the peak held since the last one, so the resource has one
//! reader: read it through `ResMut`. Another consumer of the master level
//! declares a meter of its own (`AudioGraphRes::graph().open_meter(..)`). A
//! track's level is the same: a meter on the node's output port.
//!
//! Copying audio out of the graph is a tap, not a meter: open one with
//! `AudioGraphRes::graph().open_output_tap(..)`, which returns the reader its
//! owner drains.

use bevy_ecs::prelude::*;

use tutti_graph::MeterReader;

/// The master output's peak/RMS meter, as a Bevy resource.
///
/// Derefs to [`MeterReader`]: read it with [`MeterReader::read`] (one
/// [`Level`](tutti_graph::Level) per output channel, the peak held since the
/// last read), switch it with `enable`/`disable`. Reading is lock-free from
/// any system; the plan's meter op writes it on the audio thread.
#[derive(Resource, Debug)]
pub struct MeteringRes(pub MeterReader);

impl std::ops::Deref for MeteringRes {
    type Target = MeterReader;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for MeteringRes {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
