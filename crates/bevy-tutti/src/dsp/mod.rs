//! DSP unit spawn triggers (filter, reverb, delay, chorus, compressor, gate, LFO).
//!
//! Each `Add*` component, when added to an entity, is consumed by its
//! sibling system in [`systems`], which builds the corresponding tutti
//! unit, adds it to the graph, and inserts the entity-as-node shape:
//!
//! ```text
//! AudioNode(id)        — wraps the tutti NodeId
//! NodeKind::*          — dispatch tag for parameter reconcilers
//! <typed param components> — Frequency / FilterQ / DelayTime / …
//! ```

use bevy_app::{App, Plugin, Update};
// `IntoScheduleConfigs` (for `.in_set`) and the other schedule traits come in
// via the ECS prelude; needed for the unconditional LFO spawn systems below as
// well as the `dsp`-gated effect systems.
use bevy_ecs::prelude::*;

mod components;
mod systems;

#[allow(deprecated)]
pub use components::AddLfo;
#[cfg(feature = "dsp")]
#[allow(deprecated)]
pub use components::{AddChorus, AddCompressor, AddDelay, AddFilter, AddGate, AddReverb};

pub use systems::{dsp_lfo_system, spawn_lfo_nodes};
#[cfg(feature = "dsp")]
pub use systems::{
    dsp_chorus_system, dsp_compressor_system, dsp_delay_system, dsp_filter_system,
    dsp_gate_system, dsp_reverb_system, spawn_chorus_nodes, spawn_compressor_nodes,
    spawn_delay_nodes, spawn_filter_nodes, spawn_gate_nodes, spawn_reverb_nodes,
};

/// Bevy plugin: DSP unit spawn systems.
///
/// `AddLfo` is unconditional; the rest are gated behind the `dsp` feature.
/// Per-parameter reconciliation lives in [`crate::graph`].
pub struct TuttiDspPlugin;

impl Plugin for TuttiDspPlugin {
    fn build(&self, app: &mut App) {
        use crate::graph::reconcile::GraphReconcileSystems;

        // Register the marker + construction components for reflection. The
        // param components are registered by `register_audio_node_types`
        // (graph/mod.rs); here we cover the markers + LFO authored data.
        crate::graph::register_audio_node_types(app);

        // LFO: both the marker spawner and the deprecated `AddLfo` shim, in
        // the Spawn set so the deferred `AudioNode` insert lands before the
        // Despawn set's `Added<AudioNode>` bookkeeping.
        app.add_systems(
            Update,
            (
                spawn_lfo_nodes.in_set(GraphReconcileSystems::Spawn),
                dsp_lfo_system.in_set(GraphReconcileSystems::Spawn),
            ),
        );

        #[cfg(feature = "dsp")]
        {
            use crate::graph::reconcile::{reconcile_reverb_params, reconcile_unit_params};

            // Marker-driven spawners (preferred). In the Spawn set.
            app.add_systems(
                Update,
                (
                    spawn_compressor_nodes,
                    spawn_gate_nodes,
                    spawn_filter_nodes,
                    spawn_reverb_nodes,
                    spawn_delay_nodes,
                    spawn_chorus_nodes,
                )
                    .in_set(GraphReconcileSystems::Spawn),
            );

            app.add_systems(
                Update,
                (
                    // Deprecated `Add*` shim spawners, also in the Spawn set.
                    dsp_compressor_system.in_set(GraphReconcileSystems::Spawn),
                    dsp_gate_system.in_set(GraphReconcileSystems::Spawn),
                    dsp_filter_system.in_set(GraphReconcileSystems::Spawn),
                    dsp_reverb_system.in_set(GraphReconcileSystems::Spawn),
                    dsp_delay_system.in_set(GraphReconcileSystems::Spawn),
                    dsp_chorus_system.in_set(GraphReconcileSystems::Spawn),
                    // One generic param reconciler for every effect with
                    // `AudioUnit::set` (filter / ladder / delay / chorus /
                    // flanger / phaser / compressor / gate / limiter /
                    // brickwall). Reverb keeps its own crossfade-rebuild path.
                    reconcile_unit_params.in_set(GraphReconcileSystems::Params),
                    reconcile_reverb_params.in_set(GraphReconcileSystems::Params),
                    // Per-node param-epoch bump for the dsp family (filter /
                    // delay / dynamics / …). Reads `Changed<T>`, not graph
                    // state, so it needs no set ordering.
                    crate::graph::param_epoch::bump_param_epoch_dsp,
                ),
            );
        }

        #[cfg(feature = "convolution")]
        app.add_systems(
            Update,
            crate::graph::reconcile::reconcile_convolver_params
                .in_set(crate::graph::GraphReconcileSystems::Params),
        );
    }
}
