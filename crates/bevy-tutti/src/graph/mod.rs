//! Graph reconciliation: entity-as-node component changes → tutti graph ops.
//!
//! The keystone duty for `bevy-tutti`. Other duties (DSP, automation,
//! plugin host, sampler) all schedule against [`reconcile::GraphReconcileSystems`]
//! to interleave their per-frame work between spawn → params → despawn → commit.
//!
//! Sub-concepts:
//! - [`reconcile`] — `SpawnAudioNode` extension, `Volume`/`Pan`/`Mute` reconcile,
//!   per-effect param reconcilers, `GraphReconcileSystems` ordering.
//! - [`sidechain`] — `SidechainOf` relationship → port-1 wiring.
//! - [`routing`] — `AudioFeedsTo` relationship → general port-to-port wiring.
//! - [`pending_load`] — sampler pending-load promotion (sampler-gated).
//! - [`scheduled`] — time-delayed MIDI dispatch (midi-gated).

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;

pub mod reconcile;
pub mod routing;
pub mod sidechain;

#[cfg(feature = "sampler")]
pub mod pending_load;
#[cfg(feature = "midi")]
pub mod scheduled;

pub use reconcile::{
    commit_graph, crossfade_audio_node, reconcile_node_despawn, reconcile_params, GraphDirty,
    GraphReconcileSystems, SpawnAudioNode,
};
#[cfg(feature = "sampler")]
pub use reconcile::reconcile_sampler_params;
#[cfg(feature = "plugin")]
pub use reconcile::reconcile_plugin_params;
#[cfg(feature = "dsp")]
pub use reconcile::{
    reconcile_chorus_params, reconcile_compressor_params, reconcile_delay_params,
    reconcile_filter_params, reconcile_gate_params,
};

pub use routing::{reconcile_audio_routing, AudioFedBy, AudioFeedsTo};
pub use sidechain::{reconcile_sidechain_links, SidechainOf, SidechainSources};

#[cfg(feature = "sampler")]
pub use pending_load::{
    poll_wave_imports, promote_pending_samplers, PendingSamplerLoad, WaveImportQueue,
};
#[cfg(feature = "midi")]
pub use scheduled::{tick_scheduled_midi, MidiSynthMarker, ScheduledMidi};

/// Bevy plugin: graph reconciliation pipeline.
///
/// Runs the four-phase reconcile cycle every `Update`: `Spawn` → `Params`
/// → `Despawn` → `Commit`. Other plugins hook into these sets to interleave
/// their work.
pub struct TuttiGraphPlugin;

impl Plugin for TuttiGraphPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<GraphDirty>().configure_sets(
            Update,
            (
                GraphReconcileSystems::Spawn,
                GraphReconcileSystems::Params,
                GraphReconcileSystems::Despawn,
                GraphReconcileSystems::Commit,
            )
                .chain(),
        );

        app.add_systems(
            Update,
            (
                reconcile_params.in_set(GraphReconcileSystems::Params),
                reconcile_node_despawn.in_set(GraphReconcileSystems::Despawn),
                commit_graph.in_set(GraphReconcileSystems::Commit),
                reconcile_sidechain_links.in_set(GraphReconcileSystems::Spawn),
                reconcile_audio_routing.in_set(GraphReconcileSystems::Spawn),
            ),
        );

        #[cfg(feature = "sampler")]
        {
            app.init_resource::<WaveImportQueue>().add_systems(
                Update,
                (
                    reconcile_sampler_params.in_set(GraphReconcileSystems::Params),
                    poll_wave_imports,
                    promote_pending_samplers
                        .after(poll_wave_imports)
                        .in_set(GraphReconcileSystems::Spawn),
                ),
            );
        }

        #[cfg(feature = "plugin")]
        app.add_systems(
            Update,
            reconcile_plugin_params.in_set(GraphReconcileSystems::Params),
        );

        #[cfg(all(feature = "plugin", feature = "vst2"))]
        {
            use crate::vst2_load::process_pending_vst2_builds;
            app.add_systems(
                Update,
                process_pending_vst2_builds.in_set(GraphReconcileSystems::Spawn),
            );
        }

        #[cfg(feature = "midi")]
        app.add_systems(Update, tick_scheduled_midi);
    }
}
