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

// `TransportClockNode` — a bare `NodeId` for the global transport clock — lived
// here. Its whole documented purpose was letting a host hand-wire an edge with
// `graph.connect(clock.0, 0, node, 0)`, which is the imperative path the
// declarative wiring in `graph::wire` replaces. The clock now carries an entity
// like every other node, so it is named the same way everything else is, and a
// second spelling for one node is exactly the ambiguity that shape removes.
