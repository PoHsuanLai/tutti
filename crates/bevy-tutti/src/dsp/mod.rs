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
#[cfg(feature = "dsp")]
mod spawn;
mod systems;

#[cfg(feature = "dsp")]
pub use spawn::{spawn_dsp_node, AddDspNode, DspNode, SpawnParams};

#[allow(deprecated)]
pub use components::AddLfo;
#[cfg(feature = "dsp")]
#[allow(deprecated)]
pub use components::{AddChorus, AddCompressor, AddDelay, AddFilter, AddGate, AddReverb};

pub use systems::{dsp_lfo_system, spawn_lfo_nodes};
#[cfg(feature = "dsp")]
pub use systems::{
    dsp_chorus_system, dsp_compressor_system, dsp_delay_system, dsp_filter_system,
    dsp_gate_system, dsp_reverb_system,
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
            )
                .run_if(crate::graph::engine_ready),
        );

        #[cfg(feature = "dsp")]
        {
            use crate::core::ecs::{
                ChorusNode, CompressorNode, DelayNode, FilterNode, GateNode, ReverbNode,
            };
            use crate::graph::reconcile::{reconcile_reverb_params, reconcile_unit_params};
            use spawn::AddDspNode;

            // Marker-driven spawners (preferred). One generic `spawn_dsp_node::<T>`
            // per node type, registered via the `AddDspNode` App ext (the
            // `AddAudioSource` pattern). All land in the Spawn set.
            app.add_dsp_node::<CompressorNode>()
                .add_dsp_node::<GateNode>()
                .add_dsp_node::<FilterNode>()
                .add_dsp_node::<ReverbNode>()
                .add_dsp_node::<DelayNode>()
                .add_dsp_node::<ChorusNode>();

            // Deprecated `Add*` shim spawners, also in the Spawn set. Gated on
            // `engine_ready` since each builds against `TuttiGraphRes`.
            app.add_systems(
                Update,
                (
                    dsp_compressor_system.in_set(GraphReconcileSystems::Spawn),
                    dsp_gate_system.in_set(GraphReconcileSystems::Spawn),
                    dsp_filter_system.in_set(GraphReconcileSystems::Spawn),
                    dsp_reverb_system.in_set(GraphReconcileSystems::Spawn),
                    dsp_delay_system.in_set(GraphReconcileSystems::Spawn),
                    dsp_chorus_system.in_set(GraphReconcileSystems::Spawn),
                )
                    .run_if(crate::graph::engine_ready),
            );

            // Graph-touching param reconcilers (take `ResMut<TuttiGraphRes>`):
            // gated on `engine_ready` so they don't run — and don't panic on a
            // missing resource — when the engine failed to build.
            app.add_systems(
                Update,
                (
                    // One generic param reconciler for every effect with
                    // `AudioUnit::set` (filter / ladder / delay / chorus /
                    // flanger / phaser / compressor / gate / limiter /
                    // brickwall). Reverb keeps its own crossfade-rebuild path.
                    reconcile_unit_params.in_set(GraphReconcileSystems::Params),
                    reconcile_reverb_params.in_set(GraphReconcileSystems::Params),
                )
                    .run_if(crate::graph::engine_ready),
            );
            // Pure change-detection bump: reads `Changed<T>`, never the graph,
            // so it stays ungated (mirrors the ungated core epoch bump).
            app.add_systems(Update, crate::graph::param_epoch::bump_param_epoch_dsp);
        }

        #[cfg(feature = "convolution")]
        app.add_systems(
            Update,
            crate::graph::reconcile::reconcile_convolver_params
                .in_set(crate::graph::GraphReconcileSystems::Params)
                .run_if(crate::graph::engine_ready),
        );
    }
}
