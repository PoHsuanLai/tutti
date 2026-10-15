//! The frame-rate modulation driver that lanes replaced (doc 014 §10).
//! **Unscheduled**: nothing adds it to an `App`, and PR 5 deletes it.
//!
//! [`TuttiModulationPlugin`](super::TuttiModulationPlugin) runs modulation
//! on control lanes (`tutti_nodes::Modulation`). This module keeps the old
//! driver compiling until then: a [`ModulationMatrix`] of per-frame
//! accumulators written into node cells, the per-sample route path
//! ([`audio_rate`]) and the target registry. A host that still needs it adds
//! [`LegacyModulationPlugin`] itself, and gets the defects doc 014 lists
//! (M1–M3, M9–M11): its routes do not reach an export, a rebuild reseeds the
//! base, and its writes to a node's cell race the param path.

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
pub(crate) use target::ParamSetTargets;
pub use target::{ModBusRes, ModParamsHandle, ModTargetRegistry, ModTargetResolver};

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;

use crate::graph::{engine_ready, GraphReconcileSystems};

/// The old driver: the [`ModulationMatrix`], the [`ModTargetRegistry`], the
/// built-in [`ModSource`] kind and the systems that drive them, plus the
/// audio-rate route reconciler. **Not** added by `TuttiPlugin` or
/// [`TuttiModulationPlugin`](super::TuttiModulationPlugin); never add both.
///
/// [`rebuild`] runs before [`GraphReconcileSystems::Params`]; [`drive`] runs
/// in `Params`, writing `base + Σ layers` into the node cells.
pub struct LegacyModulationPlugin;

impl Plugin for LegacyModulationPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<audio_rate::AudioRateRoutes>();
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
        // answer must exist before it is read.
        app.configure_sets(
            Update,
            ModSourceSystems::MarkDirty.before(ModSourceSystems::Collect),
        );

        app.add_systems(
            Update,
            (
                source::clear_collected.before(ModSourceSystems::MarkDirty),
                source::ensure_rate_cells.before(ModSourceSystems::MarkDirty),
                source::mark_dirty_on_route_change.in_set(ModSourceSystems::MarkDirty),
                rebuild
                    .after(ModSourceSystems::Collect)
                    .before(GraphReconcileSystems::Params),
                drive.in_set(GraphReconcileSystems::Params),
                audio_rate::ensure_source_nodes
                    .before(audio_rate::reconcile_audio_rate)
                    .in_set(GraphReconcileSystems::Spawn),
                audio_rate::reconcile_audio_rate.in_set(GraphReconcileSystems::Spawn),
            )
                .run_if(engine_ready),
        );

        app.add_mod_source::<ModSource>();
    }
}
