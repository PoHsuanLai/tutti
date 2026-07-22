//! Transport's Bevy surface: the `TransportRes` resource, its engine-claim
//! handoff, and `TuttiTransportPlugin`.
//!
//! Co-located with the transport subsystem (rather than in a central ECS hub),
//! matching the per-subsystem plugin shape the rest of tutti follows. Only
//! compiled with the `bevy_ecs` feature.
//!
//! Construction stays in bevy-tutti's `build_into` (the transport manager Arc is
//! born mid-sequence and shared with the RT callback) — this module owns only the
//! Bevy-side wrapper + claim, not the RT wiring.

use bevy_app::{App, Plugin};
use bevy_ecs::prelude::*;

use crate::TransportHandle;

/// Lock-free transport handle (play/stop/seek/tempo/loop).
#[derive(Resource, Clone)]
pub struct TransportRes(pub TransportHandle);

impl std::ops::Deref for TransportRes {
    type Target = TransportHandle;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Transient handoff: the built transport handle. Inserted by `build_into`;
/// claimed into [`TransportRes`] by [`TuttiTransportPlugin`]'s `build()`.
#[derive(Resource)]
pub struct PendingTransport(pub Option<TransportHandle>);

/// Graph address of the global [`TransportClock`](crate::TransportClock) node.
///
/// The clock emits the current beat on two output ports — port 0 whole beats,
/// port 1 the fraction (see [`crate::transport::BEAT_PORTS`]). Beat-driven
/// nodes take those as inputs, so they need the clock's `NodeId` to wire an
/// edge to it:
///
/// ```ignore
/// graph.connect(clock.0, 0, node, 0);
/// graph.connect(clock.0, 1, node, 1);
/// ```
///
/// Re-published on device-switch graph rebuilds, since the rebuilt clock is a
/// different node.
#[derive(Resource, Clone, Copy, Debug)]
pub struct TransportClockNode(pub crate::NodeId);

/// Bevy plugin: owns transport's Bevy surface. Claims [`PendingTransport`] →
/// [`TransportRes`] during plugin build (the handle already exists — `build_into`
/// ran synchronously before this plugin was added).
pub struct TuttiTransportPlugin;

impl Plugin for TuttiTransportPlugin {
    fn build(&self, app: &mut App) {
        if let Some(PendingTransport(Some(handle))) =
            app.world_mut().remove_resource::<PendingTransport>()
        {
            app.insert_resource(TransportRes(handle));
        }
    }
}
