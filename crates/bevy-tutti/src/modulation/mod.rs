//! Control-rate modulation as an ECS surface.
//!
//! An LFO is a [`ModSource`] entity; an edge from one to a parameter is a
//! [`ModRoute`] entity. Two systems keep the engine in step: one recompiles the
//! matrix when that declaration changes, the other advances every source once a
//! frame.
//!
//! ```rust,ignore
//! let lfo = commands.spawn((
//!     ModSource::new(LfoShape::Sine),
//!     ModRate::beat_synced(Hz(1.0)),
//! )).id();
//!
//! // The target declares what is modulatable and over what range; the engine
//! // does not invent a cutoff's sensible bounds.
//! commands.entity(filter).insert(
//!     ModParamRange::default().with(ParamAddr::Unit(UnitParam::Cutoff), 1000.0, 20.0, 20000.0),
//! );
//!
//! commands.spawn(
//!     ModRoute::new(lfo, filter, ParamAddr::Unit(UnitParam::Cutoff)).with_depth(Depth(0.5)),
//! );
//! ```
//!
//! # The single-writer rule
//!
//! Modulation writes a node's own atomic, and so does a plain param reconciler.
//! Both writing means whichever runs last wins, which is a scheduling accident
//! rather than a decision. [`ModulationMatrix::is_modulated`] settles it: a
//! reconciler asks before writing, and routes an authored change through
//! [`set_base`](ModulationMatrix::set_base) when the answer is yes, so the value
//! lands *under* the modulation instead of fighting it.
//!
//! # Cascading — modulating a source's own rate
//!
//! A [`ModSource`] entity is itself routable. Declare its
//! [`UnitParam::Rate`](tutti_types::UnitParam::Rate) modulatable and route to
//! it like any other param:
//!
//! ```rust,ignore
//! let carrier = commands.spawn((
//!     ModSource::new(LfoShape::Sine),
//!     ModRate::free_running(Hz(2.0)),
//!     ModParamRange::default().with(ParamAddr::Unit(UnitParam::Rate), 2.0, 2.0, 10.0),
//! )).id();
//!
//! commands.spawn(ModRoute::new(slow_lfo, carrier, ParamAddr::Unit(UnitParam::Rate)));
//! ```
//!
//! A source carries no `AudioNode`, so the registry's downcast path cannot
//! serve this; the resolver tries a source's rate first and hands back an
//! accumulator mirroring into that entity's [`ModRateCell`] — the same cell the
//! running modulator reads its frequency from. `ModRateCell` is added
//! automatically when a route asks for one, and is a *component* precisely so it
//! outlives the rebuilds that reconstruct every source.
//!
//! Read the live rate with [`ModRateCell::frequency`];
//! [`ModRate::frequency`](ModRate) stays what the user authored.
//!
//! # Delivery: per-frame scalar, or beat-evaluated curve
//!
//! By default the driver samples each source once a frame and writes a scalar.
//! A route can instead ask for its source to be installed as a [`Curve`] the
//! *sink* evaluates:
//!
//! ```rust,ignore
//! commands.spawn(ModRoute::new(lfo, plugin_param, addr).as_curve());
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
//! `tutti_units::LfoNode` in beat-synced mode and wire its
//! [`BEAT_PORTS`](tutti_core::transport::BEAT_PORTS) inputs to the transport
//! clock, whose entity is [`EngineNodes::clock`](crate::graph::EngineNodes):
//!
//! ```rust,ignore
//! commands.spawn_audio_node(LfoNode::new().with_beat_sync(Hz(1.0)))
//!     .insert(AudioSources(vec![
//!         AudioSource::Node { entity: nodes.clock, port: 0 },
//!         AudioSource::Node { entity: nodes.clock, port: 1 },
//!     ]));
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
//! Resolving a param to an accumulator needs a downcast to a concrete node type
//! (see [`target`]), so an app registers the node types it modulates:
//!
//! ```rust,ignore
//! app.world_mut()
//!     .resource_mut::<ModTargetRegistry>()
//!     .register::<tutti_units::Compressor>();
//! ```

pub mod components;
pub mod driver;
pub mod source;
pub mod target;

pub use components::{
    CurveType, LfoShape, ModParamRange, ModRate, ModRoute, ModSource, ParamRange, Polarity,
};
pub use driver::{drive, rebuild, ModulationMatrix, ParamKey};
pub use source::{
    CollectedModSources, ModRateCell, ModSourceAppExt, ModSourceKind, ModSourceSystems,
};
pub use target::{ModBusRes, ModTargetRegistry, ModTargetResolver};

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
        app.init_resource::<ModulationMatrix>()
            .init_resource::<ModTargetRegistry>()
            .init_resource::<CollectedModSources>()
            .init_resource::<ModBusRes>();

        app.register_type::<ModSource>()
            .register_type::<ModRate>()
            .register_type::<ModRoute>()
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
                rebuild
                    .after(ModSourceSystems::Collect)
                    .before(GraphReconcileSystems::Params),
                drive.in_set(GraphReconcileSystems::Params),
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
