//! ECS wrappers for the transport and the metronome.
//!
//! Both handles are born in [`build_into`](crate::engine::build_into) — the
//! transport manager `Arc` is shared with the RT callback as it is built — and
//! inserted from there.

use bevy_ecs::prelude::*;
use std::sync::Arc;

use tutti_core::transport::{ClickState, Transport};

/// The live transport: `.0.motion` for transitions, `.0.settings` for values.
#[derive(Resource, Clone)]
pub struct TransportRes(pub Transport);

impl std::ops::Deref for TransportRes {
    type Target = Transport;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

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

/// Graph address of the global [`TransportClock`](tutti_core::TransportClock) node.
///
/// The clock emits the current beat on two output ports — port 0 whole beats,
/// port 1 the fraction (see [`tutti_core::transport::BEAT_PORTS`]). Beat-driven
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
pub struct TransportClockNode(pub tutti_core::NodeId);
