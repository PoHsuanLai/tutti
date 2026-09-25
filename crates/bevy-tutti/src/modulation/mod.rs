//! Control-rate modulation as an ECS surface.
//!
//! An LFO is a [`ModSource`] entity; an edge from one to a parameter is a
//! [`ModRoute`] entity. Two systems keep the engine in step: one recompiles the
//! matrix when that declaration changes, the other advances every source once a
//! frame.
//!
//! ```rust
//! use bevy_app::prelude::*;
//! use bevy_ecs::prelude::*;
//! use bevy_tutti::graph::{AudioGraphRes, CapturedControls, GraphReconcilePlugin, TransportRes};
//! use bevy_tutti::modulation::*;
//! use bevy_tutti::AudioEngineState;
//! use tutti_core::AudioUnit as _;
//! use tutti_core::transport::Transport;
//! use tutti_core::SampleRate;
//! use tutti_types::{BeatDuration, Depth, ParamAddr, UnitParam};
//! use tutti_nodes::{DistortionNode, ShapeKind};
//!
//! let mut graph = AudioGraphRes::unattached(0, 1);
//! graph.set_sample_rate(SampleRate(48_000.0));
//!
//! let mut app = App::new();
//! app.insert_resource(graph);
//! app.insert_resource(TransportRes(Transport::new(48_000.0)));
//! app.insert_resource(AudioEngineState::Running);
//! app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));
//! // The host names the node types it modulates — see the last section. Before
//! // the node goes in: the registry is read once, when the node's controls are
//! // captured.
//! app.world_mut()
//!     .resource_mut::<ModTargetRegistry>()
//!     .register::<DistortionNode>();
//!
//! // A unit pushed by hand is bound the way `spawn_audio_node` binds one: its
//! // controls captured first, then the entity bound to the node.
//! let unit = DistortionNode::new(ShapeKind::Tanh, 5.0);
//! let controls = CapturedControls::capture(app.world(), &unit);
//! let node = app.world_mut().resource_mut::<AudioGraphRes>().insert(unit);
//!
//! let lfo = app.world_mut().spawn((
//!     ModSource::new(LfoShape::Sine),
//!     ModSourceRate::beat_synced(BeatDuration(1.0)),
//! )).id();
//!
//! // The target declares what is modulatable and over what range; the engine
//! // does not invent a param's sensible bounds.
//! let mut drive = app.world_mut().spawn(
//!     ModParamRange::default().with(ParamAddr::Unit(UnitParam::Drive), 5.0, 0.0, 10.0),
//! );
//! controls.bind(&mut drive, node);
//! let drive = drive.id();
//!
//! app.world_mut().spawn(
//!     ModRoute::new(lfo, drive, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.5)),
//! );
//! app.update();
//!
//! // The route bound to a live accumulator. Had `register::<DistortionNode>`
//! // been forgotten, the route would still be well-formed and bind to nothing.
//! let matrix = app.world().resource::<ModulationMatrix>();
//! assert!(matrix.is_modulated(drive, ParamAddr::Unit(UnitParam::Drive)));
//! ```
//!
//! # The single-writer rule
//!
//! Modulation writes a node's own atomic, and so does a plain param reconciler.
//! Both writing means whichever runs last wins, which is a scheduling accident
//! rather than a decision. [`ModulationMatrix::is_modulated`] settles it: a
//! reconciler asks before writing, and routes an authored change through
//! `set_base` when the answer is yes, so the value
//! lands *under* the modulation instead of fighting it.
//!
//! # Cascading — modulating a source's own rate
//!
//! A [`ModSource`] entity is itself routable. Declare its
//! [`UnitParam::Rate`](tutti_types::UnitParam::Rate) modulatable and route to
//! it like any other param:
//!
//! ```rust
//! use bevy_app::prelude::*;
//! use bevy_ecs::prelude::*;
//! use bevy_tutti::graph::{AudioGraphRes, GraphReconcilePlugin, TransportRes};
//! use bevy_tutti::modulation::*;
//! use bevy_tutti::AudioEngineState;
//! use tutti_core::transport::Transport;
//! use tutti_types::{Hz, ParamAddr, UnitParam};
//!
//! let mut app = App::new();
//! app.insert_resource(AudioGraphRes::unattached(0, 1));
//! app.insert_resource(TransportRes(Transport::new(48_000.0)));
//! app.insert_resource(AudioEngineState::Running);
//! app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));
//!
//! let slow_lfo = app.world_mut().spawn((
//!     ModSource::new(LfoShape::Sine),
//!     ModSourceRate::free_running(Hz(0.2)),
//! )).id();
//!
//! let carrier = app.world_mut().spawn((
//!     ModSource::new(LfoShape::Sine),
//!     ModSourceRate::free_running(Hz(2.0)),
//!     ModParamRange::default().with(ParamAddr::Unit(UnitParam::Rate), 2.0, 2.0, 10.0),
//! )).id();
//!
//! app.world_mut().spawn(ModRoute::new(slow_lfo, carrier, ParamAddr::Unit(UnitParam::Rate)));
//! app.update();
//!
//! // No `register` was needed: a source carries no `AudioNode`, so the resolver
//! // tries its rate first and adds the `ModRateCell` the modulator reads from.
//! assert!(app.world().get::<ModRateCell>(carrier).is_some());
//! ```
//!
//! A source carries no `AudioNode`, so the registry's capture path cannot
//! serve this; the resolver tries a source's rate first and hands back an
//! accumulator mirroring into that entity's [`ModRateCell`] — the same cell the
//! running modulator reads its frequency from. `ModRateCell` is added
//! automatically when a route asks for one, and is a *component* precisely so it
//! outlives the rebuilds that reconstruct every source.
//!
//! Read the live rate with [`ModRateCell::frequency`]; [`ModSourceRate`]'s own clock
//! stays what the user authored. Only [`ModClock::Free`] gets a cell — the cell
//! is a `Param<Hz>`, and a synced span is neither an `Hz` nor an f32.
//!
//! # Delivery: per-frame scalar, or beat-evaluated curve
//!
//! By default the driver samples each source once a frame and writes a scalar.
//! A route can instead ask for its source to be installed as a `Curve` the
//! *sink* evaluates:
//!
//! ```rust
//! use bevy_ecs::prelude::*;
//! use bevy_tutti::modulation::{ModDelivery, ModRoute};
//! use tutti_types::{ParamAddr, UnitParam};
//!
//! # let mut world = World::new();
//! # let lfo = world.spawn_empty().id();
//! # let plugin_param = world.spawn_empty().id();
//! # let addr = ParamAddr::Unit(UnitParam::Cutoff);
//! let route = ModRoute::new(lfo, plugin_param, addr).per_block();
//! // A **request**, not a guarantee: honoured only if the source kind has a
//! // curve form and the sink accepts one, else it falls back to a scalar.
//! assert_eq!(route.delivery, ModDelivery::PerBlock);
//! ```
//!
//! Worth asking for only when the sink reads faster than the frame rate — a
//! plugin's per-block parameter producer traces a smooth ramp where a frame
//! scalar gives a staircase. It is a **request**: honoured only if the source
//! kind has a curve form (see
//! [`ModSourceKind::build_curve`]) *and* the sink accepts one. Anything else
//! falls back to scalar delivery, which is always correct.
//!
//! Native params always fall back — [`AtomicTarget`](tutti_mod::AtomicTarget)
//! collapses at a fixed beat, so a curve stored there would never move. Reaching
//! a sink that does accept curves means supplying it with
//! [`ModTargetRegistry::insert_target`].
//!
//! # The per-sample path is a different route entirely
//!
//! Both deliveries above are this matrix's, and neither is sample-accurate: the
//! scalar is written once a frame, and a curve is only as fine as the sink that
//! samples it. For a genuinely per-sample modulator, don't route at all — spawn
//! `tutti_nodes::LfoNode` in beat-synced mode and wire its
//! [`BEAT_PORTS`](tutti_core::transport::BEAT_PORTS) inputs to the transport
//! clock, whose entity is [`EngineNodes::clock`](crate::graph::EngineNodes):
//!
//! ```rust
//! use bevy_app::prelude::*;
//! use bevy_ecs::prelude::*;
//! use bevy_tutti::prelude::*;
//! use tutti_core::transport::{TransportClock, BEAT_PORTS};
//! use tutti_types::BeatDuration;
//! use tutti_nodes::{LfoNode, LfoShape};
//!
//! fn wire_lfo_to_clock(mut commands: Commands, nodes: Res<EngineNodes>) {
//!     commands
//!         .spawn_audio_node(LfoNode::new(LfoShape::Sine).with_beat_sync(BeatDuration(1.0)))
//!         .insert(PortSources(vec![
//!             PortSource::Node { entity: nodes.clock, port: 0 },
//!             PortSource::Node { entity: nodes.clock, port: 1 },
//!         ]));
//! }
//!
//! let transport = Transport::new(48_000.0);
//! let mut graph = AudioGraphRes::headless(0, 2);
//! let clock_id = graph.insert(TransportClock::new(transport.clock_links(), 48_000.0));
//!
//! let mut app = App::new();
//! app.insert_resource(graph);
//! app.insert_resource(AudioEngineState::Running);
//! app.add_plugins(GraphReconcilePlugin);
//! let clock = app.world_mut().spawn(clock_id).id();
//! app.insert_resource(EngineNodes { clock, click: clock });
//! app.insert_resource(TransportRes(transport));
//! app.add_systems(Startup, wire_lfo_to_clock);
//! app.update();
//!
//! let lfo = app
//!     .world_mut()
//!     .query::<&AudioNode>()
//!     .iter(app.world())
//!     .copied()
//!     .find(|id| *id != clock_id)
//!     .unwrap();
//! let graph = app.world().resource::<AudioGraphRes>();
//! for port in 0..BEAT_PORTS {
//!     assert_eq!(graph.source(lfo, port), GraphSource::Node(clock_id, port));
//! }
//! ```
//!
//! That is the same `tutti_mod::Lfo` this matrix drives, under an audio-rate
//! adapter instead of a frame-rate driver — one modulator, a different tier.
//! What it gives up is the matrix: depth/polarity/range shaping, layered
//! accumulation onto one param, and runtime re-routing are all this module's,
//! and a hand-wired node participates in none of them. Reach for it when the
//! staircase is audible; stay here otherwise.
//!
//! # What the host must supply
//!
//! Resolving a param to an accumulator needs the concrete node type (see
//! [`target`]), so an app registers the node types it modulates — before it
//! spawns them, since the registry is read once per node, as it is inserted:
//!
//! ```rust
//! use bevy_app::prelude::*;
//! use bevy_tutti::modulation::{ModTargetRegistry, TuttiModulationPlugin};
//!
//! let mut app = App::new();
//! app.add_plugins(TuttiModulationPlugin);
//! app.world_mut()
//!     .resource_mut::<ModTargetRegistry>()
//!     .register::<tutti_nodes::CompressorNode>();
//! ```

pub mod audio_rate;
pub mod components;
pub mod driver;
pub mod source;
pub mod target;

pub use components::{
    CurveType, LfoShape, ModClock, ModDelivery, ModParamRange, ModRoute, ModSource, ModSourceRate,
    ParamRange, Polarity,
};
pub use driver::{drive, rebuild, ModulationMatrix, ParamKey};
pub use source::{
    CollectedModSources, ModRateCell, ModSourceAppExt, ModSourceKind, ModSourceSystems,
};
pub use target::{ModBusRes, ModParamsHandle, ModTargetRegistry, ModTargetResolver};

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;

use crate::graph::{engine_ready, GraphReconcileSystems};

/// Control-rate modulation: the matrix, the target registry, and the two
/// systems that drive them.
///
/// Ordering matches what the two systems mean. [`rebuild`] runs before
/// `Params`, so the frame's routing is current before anything reads it.
/// [`drive`] runs *inside* `Params` and last, because it flushes
/// `base + Σ offsets` into the node atomics — a param reconciler that ran after
/// it would overwrite a modulated value with a static one.
pub struct TuttiModulationPlugin;

impl Plugin for TuttiModulationPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<audio_rate::AudioRateChains>();
        app.init_resource::<ModulationMatrix>()
            .init_resource::<ModTargetRegistry>()
            .init_resource::<CollectedModSources>()
            .init_resource::<ModBusRes>();

        app.register_type::<ModSource>()
            .register_type::<ModSourceRate>()
            .register_type::<ModClock>()
            .register_type::<ModRoute>()
            .register_type::<ModDelivery>()
            .register_type::<ModParamRange>();

        // `Collect` builds only when `MarkDirty` said something moved, so the
        // answer must exist before it is read — without this the two sets are
        // unordered and the build could run a frame early against a stale flag.
        app.configure_sets(
            Update,
            ModSourceSystems::MarkDirty.before(ModSourceSystems::Collect),
        );

        app.add_systems(
            Update,
            (
                source::clear_collected.before(ModSourceSystems::MarkDirty),
                // Before `MarkDirty` so that inserting a cell is itself seen as
                // a change this frame, and before `Collect` so the source is
                // built already reading it.
                source::ensure_rate_cells.before(ModSourceSystems::MarkDirty),
                // A route or range change also triggers `rebuild`, which drains
                // the collected sources — so it must raise the same flag, or
                // the rebuild resolves against an empty registry and drops
                // every route. Registered once, not per kind.
                source::mark_dirty_on_route_change.in_set(ModSourceSystems::MarkDirty),
                rebuild
                    .after(ModSourceSystems::Collect)
                    .before(GraphReconcileSystems::Params),
                drive.in_set(GraphReconcileSystems::Params),
                // Audio-rate delivery builds a *graph*, so it runs in the graph
                // reconcile phase rather than beside `rebuild`. Source nodes
                // first: a shaper cannot be pointed at a node that does not
                // exist yet.
                audio_rate::ensure_source_nodes
                    .before(audio_rate::reconcile_audio_rate)
                    .in_set(GraphReconcileSystems::Spawn),
                audio_rate::reconcile_audio_rate.in_set(GraphReconcileSystems::Spawn),
            )
                // All reach into the audio graph — `rebuild` to resolve a node,
                // `drive` to write its atomics — so none means anything
                // without a running engine.
                .run_if(engine_ready),
        );

        // The built-in kind. An app adds its own with `add_mod_source::<K>()`;
        // registering here means the common case needs no setup at all.
        app.add_mod_source::<ModSource>();
    }
}
