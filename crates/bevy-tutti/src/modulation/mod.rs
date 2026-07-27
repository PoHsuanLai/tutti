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
pub use source::{CollectedModSources, ModSourceAppExt, ModSourceKind, ModSourceSystems};
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
