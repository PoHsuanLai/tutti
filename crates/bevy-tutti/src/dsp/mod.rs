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
#[cfg(feature = "dsp")]
use bevy_ecs::prelude::*;

mod components;
mod systems;

pub use components::AddLfo;
#[cfg(feature = "dsp")]
pub use components::{AddChorus, AddCompressor, AddDelay, AddFilter, AddGate, AddReverb};

pub use systems::dsp_lfo_system;
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
        app.add_systems(Update, dsp_lfo_system);

        #[cfg(feature = "dsp")]
        {
            use crate::graph::reconcile::{
                reconcile_chorus_params, reconcile_compressor_params, reconcile_delay_params,
                reconcile_filter_params, reconcile_gate_params, GraphReconcileSystems,
            };

            app.add_systems(
                Update,
                (
                    dsp_compressor_system,
                    dsp_gate_system,
                    dsp_filter_system,
                    dsp_reverb_system,
                    dsp_delay_system,
                    dsp_chorus_system,
                    reconcile_filter_params.in_set(GraphReconcileSystems::Params),
                    reconcile_delay_params.in_set(GraphReconcileSystems::Params),
                    reconcile_chorus_params.in_set(GraphReconcileSystems::Params),
                    reconcile_compressor_params.in_set(GraphReconcileSystems::Params),
                    reconcile_gate_params.in_set(GraphReconcileSystems::Params),
                ),
            );
        }
    }
}
