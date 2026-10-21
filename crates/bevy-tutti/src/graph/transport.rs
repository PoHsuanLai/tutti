//! ECS wrappers for the transport and the metronome, plus the entities of the
//! nodes the engine builds for itself.
//!
//! All three are born in [`build_into`](crate::engine::build_into) — the
//! transport manager `Arc` is shared with the RT callback as it is built — and
//! inserted from there.
//!
//! [`TransportRes`] is time as a **value**, read per frame, or through
//! [`timeline`](TransportRes::timeline) by code outside the graph. A graph
//! node reads it per frame from its block's `Env`, with nothing wired into
//! it (the metronome, a beat-synced LFO, an automation lane).

use bevy_ecs::prelude::*;
use std::sync::Arc;

use tutti_core::transport::{ClickState, Timeline, Transport};

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

impl TransportRes {
    /// Returns a [`Timeline`] handle on the live transport, for a reader
    /// outside the graph.
    ///
    /// A graph node does not use this: it reads the transport of each frame
    /// from its block's `Env` (`tutti_graph::Env::transport_at`), which
    /// carries every change inside the block. This handle is for code with no
    /// `Env`, such as a host's own sequencer thread or a UI: hand it over
    /// **once**, at install time, and read it whenever that code runs, rather
    /// than having a system push the beat every frame.
    ///
    /// The clone shares state rather than snapshotting it: every field
    /// [`Timeline`] reads — the beat, the tempo, the rolling flag — lives behind
    /// an `Arc` over an atomic, so what the source holds is another reference to
    /// the live transport, not a copy of this frame's values.
    ///
    /// ```rust
    /// use bevy_app::prelude::*;
    /// use bevy_ecs::prelude::*;
    /// use bevy_tutti::prelude::*;
    /// use std::sync::Arc;
    ///
    /// /// Stands in for a reader of the live transport outside the graph
    /// /// (a host's sequencer thread): it holds the handle and reads it
    /// /// when it runs.
    /// #[derive(Resource)]
    /// struct Sequencer(Arc<dyn Timeline>);
    ///
    /// /// Hand the handle over **once**, at install time — not a beat per frame.
    /// fn install(transport: Res<TransportRes>, mut commands: Commands) {
    ///     commands.insert_resource(Sequencer(transport.timeline()));
    /// }
    ///
    /// let mut app = App::new();
    /// app.insert_resource(TransportRes(Transport::new(48_000.0)));
    /// app.add_systems(Startup, install);
    /// app.update();
    ///
    /// // The clone shares state rather than snapshotting it: a tempo set after
    /// // the handle was taken is visible through it.
    /// app.world().resource::<TransportRes>().settings.set_tempo(128.0);
    /// assert_eq!(app.world().resource::<Sequencer>().0.tempo(), Bpm(128.0));
    /// ```
    pub fn timeline(&self) -> Arc<dyn Timeline> {
        Arc::new(self.0.clone())
    }

    /// Returns a [`TransportState`](tutti_core::transport::TransportState)
    /// handle: [`timeline`](Self::timeline) plus the
    /// live-session facts a plain timeline has no vocabulary for: whether the
    /// transport is recording, its loop region, and free-running stream time.
    ///
    /// Same seam and same sharing as `timeline`; the difference is only how much
    /// of the transport the consumer is allowed to ask about. Nothing in this
    /// repository consumes it now (a graph node, a hosted plugin's included,
    /// reads its block's `Env`); it is kept as public API for a host's own
    /// sink that needs those extras. `TransportState` is a strict supertrait of
    /// `Timeline`, so an offline render (a `Timeline`-only implementor) cannot
    /// be passed where this is wanted, which is the point: it has no answer for
    /// "am I recording".
    pub fn transport_state(&self) -> Arc<dyn tutti_core::transport::TransportState> {
        Arc::new(self.0.clone())
    }
}

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
/// Accent is not among them, and has no setter: it is derived from the meter's
/// downbeat. A standalone accent count would have to default to something —
/// and any default is wrong for some time signature.
#[derive(Resource, Clone)]
pub struct MetronomeRes(pub Arc<ClickState>);

impl std::ops::Deref for MetronomeRes {
    type Target = ClickState;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// The entity of the node [`build_into`](crate::engine::build_into) puts in
/// the graph before any host system runs: the metronome.
///
/// Not a second way to name a node — the *only* way to name this one. Every
/// other node is spawned by the host, which keeps the `Entity`
/// [`spawn_audio_node`](crate::graph::SpawnAudioNode::spawn_audio_node) hands
/// back. This one is built during engine construction, so without this
/// resource its entity is unreachable: it carries
/// [`AudioNode`](tutti_core::AudioNode) and nothing else.
///
/// That matters because [`PortSources`](crate::graph::PortSources) names
/// sources by `Entity`. An unreachable entity is an unwirable node.
///
/// It holds `Entity`, not `NodeKey`, for the reason the whole wiring layer does:
/// a declaration names entities, so a bare engine id would be unusable here.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineNodes {
    /// The [`ClickNode`](tutti_core::ClickNode) — the metronome.
    ///
    /// Its **outputs are unwired**: where the click lands is the host's
    /// declaration, like every other source.
    ///
    /// Declare it with [`MasterSources`](crate::graph::MasterSources), or feed
    /// it into a mixer with [`PortSources`](crate::graph::PortSources).
    ///
    /// It has **no inputs**: it reads the beat of every frame from its
    /// block's `Env`, so each click starts on its exact frame rather than on a
    /// block boundary.
    ///
    /// Volume, mode and meter are separate — those are atomics on
    /// [`MetronomeRes`], not graph edges.
    pub click: Entity,
}
