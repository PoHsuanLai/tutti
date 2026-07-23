//! Transport's Bevy surface: the `TransportRes` resource, its engine-claim
//! handoff, and `TuttiTransportPlugin`.
//!
//! Part of the [`crate::ecs`] hub (all of tutti-core's Bevy integration under
//! one roof); the transport value types it wraps stay in [`crate::transport`].
//! Re-exported from `tutti_core::transport` so that path keeps resolving. Only
//! compiled with the `bevy` feature.
//!
//! Construction stays in bevy-tutti's `build_into` (the transport manager Arc is
//! born mid-sequence and shared with the RT callback) — this module owns only the
//! Bevy-side wrapper + claim, not the RT wiring.

use bevy_app::{App, Plugin};
use bevy_ecs::prelude::*;
use std::sync::Arc;

use crate::transport::{ClickState, Transport};

/// The live transport: `.0.motion` for transitions, `.0.settings` for values.
#[derive(Resource, Clone)]
pub struct TransportRes(pub Transport);

impl std::ops::Deref for TransportRes {
    type Target = Transport;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Transient handoff: the built transport handle. Inserted by `build_into`;
/// claimed into [`TransportRes`] by [`TuttiTransportPlugin`]'s `build()`.
#[derive(Resource)]
pub struct PendingTransport(pub Option<Transport>);

/// Metronome control: the shared [`ClickState`] the click node reads.
///
/// Separate from [`TransportRes`]: the metronome shares no state with the
/// transport. Callers set volume/accent/mode through `ClickState`'s atomic
/// setters (`set_volume` / `set_mode` / `set_accent_every`) directly — there
/// is no fluent wrapper.
#[derive(Resource, Clone)]
pub struct MetronomeRes(pub Arc<ClickState>);

impl std::ops::Deref for MetronomeRes {
    type Target = ClickState;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Transient handoff for [`MetronomeRes`], mirroring [`PendingTransport`].
#[derive(Resource)]
pub struct PendingMetronome(pub Option<Arc<ClickState>>);

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
        if let Some(PendingMetronome(Some(handle))) =
            app.world_mut().remove_resource::<PendingMetronome>()
        {
            app.insert_resource(MetronomeRes(handle));
        }
    }
}
