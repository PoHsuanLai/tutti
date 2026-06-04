//! DSP unit spawn (filter, reverb, delay, chorus, compressor, gate, LFO).
//!
//! Spawn a marker (`CompressorNode`, `FilterNode`, …); its `#[require(...)]`
//! list inserts the param components, and the sibling system in [`systems`]
//! builds the tutti unit from them, adds it to the graph, and inserts the
//! entity-as-node shape:
//!
//! ```text
//! AudioNode(id)        — wraps the tutti NodeId
//! NodeKind::*          — dispatch tag for parameter reconcilers
//! <typed param components> — Frequency / FilterQ / DelayTime / …
//! ```

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;

mod spawn;
mod systems;

pub use spawn::{spawn_dsp_node, AddDspNode, DspNode, SpawnParams};

pub use systems::spawn_lfo_nodes;

/// Bevy plugin: DSP unit spawn systems + param reconcilers.
///
/// Registers the authoring markers, the marker-driven generic spawners
/// (`spawn_dsp_node::<T>`), the LFO spawner, and the DSP param reconcilers
/// (`reconcile_unit_params` / `reconcile_reverb_params`) + epoch bump. The
/// generic graph hub ([`tutti_core::graph::GraphReconcilePlugin`]) must be added
/// first — it configures the `GraphReconcileSystems` schedule this plugin
/// schedules against.
pub struct TuttiDspPlugin;

impl Plugin for TuttiDspPlugin {
    fn build(&self, app: &mut App) {
        use tutti_core::graph::engine_ready;
        use tutti_core::graph::GraphReconcileSystems;

        // Register the DSP param pool + authoring markers (both owned by this
        // crate now). The core graph plugin registers only the foundational
        // params (`Volume`/`Pan`/`Mute`/`NodeKind`).
        crate::dsp_params::register_param_types(app);
        crate::node_markers::register_node_markers(app);

        // LFO: bespoke marker spawner, in the Spawn set so the deferred
        // `AudioNode` insert lands before the Despawn set's `Added<AudioNode>`
        // bookkeeping.
        app.add_systems(
            Update,
            spawn_lfo_nodes
                .in_set(GraphReconcileSystems::Spawn)
                .run_if(engine_ready),
        );

        use spawn::AddDspNode as _;
        use crate::node_markers::{
            ChorusNode, CompressorNode, DelayNode, FilterNode, GateNode, ReverbNode,
        };

        // Marker-driven spawners (preferred). One generic `spawn_dsp_node::<T>`
        // per node type, registered via the `AddDspNode` App ext. All land in
        // the Spawn set.
        app.add_dsp_node::<CompressorNode>()
            .add_dsp_node::<GateNode>()
            .add_dsp_node::<FilterNode>()
            .add_dsp_node::<ReverbNode>()
            .add_dsp_node::<DelayNode>()
            .add_dsp_node::<ChorusNode>();

        // Graph-touching param reconcilers + the (ungated) epoch bump.
        super::reconcile::build(app);

        #[cfg(feature = "convolution")]
        {
            use super::pending_convolver::{promote_pending_convolvers, start_convolver_loads};
            // `start_convolver_loads` only uses `AssetServer` (not an engine
            // resource), so it stays ungated; `promote_pending_convolvers` needs
            // the graph.
            app.add_systems(Update, start_convolver_loads);
            app.add_systems(
                Update,
                promote_pending_convolvers
                    .after(start_convolver_loads)
                    .in_set(GraphReconcileSystems::Spawn)
                    .run_if(engine_ready),
            );
            app.add_systems(
                Update,
                super::reconcile::reconcile_convolver_params
                    .in_set(GraphReconcileSystems::Params)
                    .run_if(engine_ready),
            );
        }
    }
}

