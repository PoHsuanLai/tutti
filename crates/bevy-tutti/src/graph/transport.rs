//! ECS wrappers for the transport and the metronome, plus the entities of the
//! nodes the engine builds for itself.
//!
//! All three are born in [`build_into`](crate::engine::build_into) — the
//! transport manager `Arc` is shared with the RT callback as it is built — and
//! inserted from there.
//!
//! [`TransportRes`] and [`EngineNodes`] are the two halves of "time", and which
//! one a consumer wants follows from *how it reads*: [`TransportRes`] is time as
//! a **value**, read per frame or (via [`timeline`](TransportRes::timeline)) per
//! block; [`EngineNodes::clock`] is time as a **signal**, wired into a node's
//! input ports and read per sample.

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
    /// A [`Timeline`] handle for an audio-thread source to ask the beat with.
    ///
    /// **This is the seam between the two rates**, and getting it right is the
    /// difference between sample-accurate scheduling and framerate-quantised
    /// scheduling. An ECS system runs per *frame*; a beat-scheduled source needs
    /// the beat per *block*, and the two are neither equal nor aligned. So a
    /// system does not read the beat and push events — it hands over this handle
    /// once, at install time, and the source reads the beat itself every block
    /// (usually through a [`BeatCursor`](tutti_core::transport::BeatCursor),
    /// which owns the seek-epsilon and paused-case arithmetic).
    ///
    /// The clone shares state rather than snapshotting it: every field
    /// [`Timeline`] reads — the beat, the tempo, the rolling flag — lives behind
    /// an `Arc` over an atomic, so what the source holds is another reference to
    /// the live transport, not a copy of this frame's values.
    ///
    /// (`Transport::sample_rate` is a plain `f64` and *does* copy. No `Timeline`
    /// method reads it and nothing mutates it after construction, so it cannot
    /// drift — but a future `set_sample_rate`, or a `Timeline` method that reads
    /// it, would make that a live bug rather than a footnote.)
    ///
    /// ```rust
    /// use bevy_app::prelude::*;
    /// use bevy_ecs::prelude::*;
    /// use bevy_tutti::prelude::*;
    /// use std::sync::Arc;
    ///
    /// /// Stands in for a beat-scheduled audio-thread source. The real ones
    /// /// (`MidiClipSource`, an automation lane) hold the handle exactly like
    /// /// this and read it per block.
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

    /// A [`TransportState`](tutti_core::transport::TransportState) handle —
    /// [`timeline`](Self::timeline) plus the
    /// live-session facts a plain timeline has no vocabulary for: whether the
    /// transport is recording, its loop region, and free-running stream time.
    ///
    /// Same seam and same sharing as `timeline`; the difference is only how much
    /// of the transport the consumer is allowed to ask about. Use this when the
    /// sink genuinely needs those extras — a hosted plugin's `TransportInfo`
    /// carries recording and loop state, so its sources take `TransportState`
    /// — and `timeline` otherwise. `TransportState` is a strict supertrait of
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

/// The entities of the two nodes [`build_into`](crate::engine::build_into) puts
/// in the graph before any host system runs.
///
/// Not a second way to name a node — the *only* way to name these two. Every
/// other node is spawned by the host, which keeps the `Entity`
/// [`spawn_audio_node`](crate::graph::SpawnAudioNode::spawn_audio_node) hands
/// back. These two are built during engine construction, so without this
/// resource their entities are unreachable: they carry
/// [`AudioNode`](tutti_core::AudioNode) and nothing else, and a query cannot
/// tell them apart from each other.
///
/// That matters because [`PortSources`](crate::graph::PortSources) names
/// sources by `Entity`. An unreachable entity is an unwirable node.
///
/// It holds `Entity`, not `NodeId`, for the reason the whole wiring layer does:
/// a declaration names entities, so a bare engine id would be unusable here.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineNodes {
    /// The beat clock: an [`EnvClock`](tutti_core::EnvClock), which emits a
    /// `TransportClock`'s beat ports from each block's `Env` (the graph engine
    /// drives its own `TransportClock` and forbids a second in the graph).
    ///
    /// Emits the beat on [`BEAT_PORTS`](tutti_core::transport::BEAT_PORTS)
    /// output ports — **port 0 whole beats, port 1 the fraction** — which is the
    /// convention every beat-driven node reads and
    /// [`beat_from_ports`](tutti_core::transport::beat_from_ports) reassembles.
    /// The split exists because one `f32` cannot carry a musical position past
    /// beat 16384 without audible stair-stepping.
    ///
    /// Wire both ports, in that order, to a node that takes the beat as a
    /// signal — `tutti_nodes::Lfo` in beat-synced mode, or an `AutomationLaneNode`:
    ///
    /// ```rust
    /// use bevy_app::prelude::*;
    /// use bevy_ecs::prelude::*;
    /// use bevy_tutti::prelude::*;
    /// use tutti_core::transport::BEAT_PORTS;
    /// use tutti_nodes::testing::Through;
    ///
    /// /// Stands in for a beat-driven node — `tutti_nodes::Lfo` in beat-synced
    /// /// mode, or an automation lane. What matters is that it takes the beat on
    /// /// two input ports, in port order.
    /// fn beat_driven_node() -> impl tutti_core::AudioUnit {
    ///     Through::new(tutti_core::ChannelLayout::STEREO)
    /// }
    ///
    /// fn wire_to_clock(mut commands: Commands, nodes: Res<EngineNodes>) {
    ///     commands
    ///         .spawn_audio_node(beat_driven_node())
    ///         .insert(PortSources(vec![
    ///             PortSource::Node { entity: nodes.clock, port: 0 },
    ///             PortSource::Node { entity: nodes.clock, port: 1 },
    ///         ]));
    /// }
    ///
    /// // `build_into` builds the clock and inserts `EngineNodes`; a device-less
    /// // app does the same two steps by hand.
    /// let transport = Transport::new(48_000.0);
    /// let mut graph = AudioGraphRes::headless(0, 2);
    /// let clock_id = graph.insert_beat_clock();
    ///
    /// let mut app = App::new();
    /// app.insert_resource(graph);
    /// app.insert_resource(AudioEngineState::Running);
    /// app.add_plugins(GraphReconcilePlugin);
    /// let clock = app.world_mut().spawn(clock_id).id();
    /// app.insert_resource(EngineNodes { clock, click: clock });
    /// app.insert_resource(TransportRes(transport));
    /// app.add_systems(Startup, wire_to_clock);
    /// app.update();
    ///
    /// // Both beat ports reached the engine, in order. Wiring only port 0 would
    /// // stair-step past beat 16384 — which is why the split exists.
    /// let sink = app
    ///     .world_mut()
    ///     .query::<&AudioNode>()
    ///     .iter(app.world())
    ///     .copied()
    ///     .find(|id| *id != clock_id)
    ///     .unwrap();
    /// let graph = app.world().resource::<AudioGraphRes>();
    /// for port in 0..BEAT_PORTS {
    ///     assert_eq!(graph.source(sink, port), GraphSource::Node(clock_id, port));
    /// }
    /// ```
    ///
    /// **This crate wires it to one node only: the [`click`](Self::click)**,
    /// whose beat inputs `build_into` declares so every onset lands on its exact
    /// frame. bevy-tutti's own modulation reads the beat per *frame* from
    /// [`TransportRes`] and pushes it into the driver (see
    /// `modulation::driver`), trading sample accuracy for
    /// a scalar that ECS change detection can carry; a sink that wants the
    /// smooth form asks for a beat-evaluated curve instead. So this field exists
    /// for host-spawned nodes, and is the seam a host reaches for when it wants
    /// the per-sample path this crate's own modulation forgoes.
    pub clock: Entity,
    /// The [`ClickNode`](tutti_core::ClickNode) — the metronome.
    ///
    /// Its **outputs are deliberately unwired**: where the click lands is the
    /// host's declaration, like every other source. A `pipe_output` here would
    /// read like "mix the click into master" but overwrite every global output
    /// edge, so the first soundfont to load would silently disconnect the
    /// metronome.
    ///
    /// Declare it with [`MasterSources`](crate::graph::MasterSources), or feed
    /// it into a mixer with [`PortSources`](crate::graph::PortSources).
    ///
    /// Its **inputs are wired by the engine**: the entity is born with a
    /// [`PortSources`](crate::graph::PortSources) taking the beat from
    /// [`clock`](Self::clock)'s two ports, which is how each click starts on its
    /// exact frame rather than on a block boundary. Replacing that component
    /// re-points the metronome's beat; removing it leaves the click on beat 0.
    ///
    /// Volume, mode and meter are separate — those are atomics on
    /// [`MetronomeRes`], not graph edges.
    pub click: Entity,
}
