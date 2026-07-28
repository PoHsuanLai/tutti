//! ECS wrappers for the transport and the metronome.
//!
//! Both handles are born in [`build_into`](crate::engine::build_into) — the
//! transport manager `Arc` is shared with the RT callback as it is built — and
//! inserted from there.

use bevy_ecs::prelude::*;
use std::sync::Arc;

use tutti_core::transport::{ClickState, Transport};

/// The live transport. Newtype over tutti-core's [`Transport`] — every method
/// below is the engine's, reached through the `Deref`.
///
/// Three fields carry everything:
///
/// - **`.motion`** — transitions. `try_send(MotionEvent::Play)` and friends
///   *queue*; the audio thread drains them at the top of each block, so
///   `is_playing()` does not flip until something renders.
/// - **`.settings`** — values. `set_tempo`, `set_beat`, `set_recording`, and the
///   reads beside them.
/// - **`.settings.loop_span`** — the loop region. `set_range(start, end)` and
///   `set_enabled(..)` to arm it; [`range()`](tutti_core::transport::LoopSpan::range)
///   to read it back as a validated [`LoopRange`](tutti_core::transport::LoopRange),
///   or `None` if it is disabled, empty or inverted. `bounds()` gives the raw
///   pair instead, which is what a UI drawing a brace mid-drag wants — an
///   inverted pair is a legitimate transient there, which is why the setter does
///   not validate and the reader does.
///
/// Everything takes `&self` (the state is atomics), so `Res` suffices. That also
/// means **`Res<TransportRes>` never triggers change detection** — a host that
/// wants "did the tempo change this frame" diffs the value itself.
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
/// transport. Callers reach `ClickState`'s atomic setters (`set_volume` /
/// `set_mode` / `set_meter`) through the `Deref` — there is no fluent wrapper.
///
/// Accent is not among them: it is derived from the meter's downbeat, replacing
/// a standalone `accent_every` count that defaulted to 4 whatever the time
/// signature said. This doc named that setter for a while after it was removed.
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
