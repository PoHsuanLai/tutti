//! Graph reconciliation: entity-as-node component changes → tutti graph ops.
//!
//! The keystone duty for `bevy-tutti`. Other duties (DSP, automation,
//! plugin host, sampler) all schedule against [`reconcile::GraphReconcileSystems`]
//! to interleave their per-frame work between spawn → params → despawn → commit.
//!
//! Sub-concepts:
//! - [`reconcile`] — `SpawnAudioNode` extension, `Volume`/`Pan`/`Mute` reconcile,
//!   per-effect param reconcilers, `GraphReconcileSystems` ordering.
//! - [`sidechain`] — `SidechainOf` relationship → sidechain-bus port wiring.
//! - [`routing`] — `AudioFeedsTo` relationship → general port-to-port wiring.
//! - [`pending_load`] — sampler pending-load promotion (sampler-gated).
//! - [`scheduled`] — time-delayed MIDI dispatch (midi-gated).

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;

pub mod param_epoch;
pub mod reconcile;
pub mod routing;
pub mod sidechain;

#[cfg(feature = "sampler")]
pub mod pending_load;
#[cfg(feature = "convolution")]
pub mod pending_convolver;
#[cfg(feature = "midi")]
pub mod scheduled;

pub use param_epoch::{bump_param_epoch_core, NodeParamEpoch};
pub use reconcile::{
    commit_graph, crossfade_audio_node, engine_ready, reconcile_node_despawn, reconcile_params,
    GraphDirty, GraphReconcileSystems, SpawnAudioNode,
};
#[cfg(feature = "sampler")]
pub use reconcile::reconcile_sampler_params;
#[cfg(feature = "plugin")]
pub use reconcile::reconcile_plugin_params;
#[cfg(feature = "dsp")]
pub use reconcile::{reconcile_reverb_params, reconcile_unit_params};

pub use routing::{reconcile_audio_routing, AudioFedBy, AudioFeedsTo};
pub use sidechain::{
    reconcile_sidechain_links, reconcile_sidechain_remove, SidechainOf, SidechainSources,
};

#[cfg(feature = "sampler")]
pub use pending_load::{
    poll_wave_imports, promote_pending_samplers, PendingSamplerLoad, WaveImportQueue,
};
#[cfg(feature = "convolution")]
pub use pending_convolver::{
    promote_pending_convolvers, start_convolver_loads, PendingConvolverLoad,
};
#[cfg(feature = "convolution")]
pub use reconcile::reconcile_convolver_params;
#[cfg(feature = "midi")]
pub use scheduled::{tick_scheduled_midi, MidiSynthMarker, ScheduledMidi};

/// Register every reflectable entity-as-node component (params, construction
/// data, and authoring markers) for the type registry.
///
/// The `tutti-core` param components (`Volume`, `Pan`, …) and the B7 markers
/// (`CompressorNode`, …) all derive `Reflect` but were registered nowhere;
/// this is the single registration slot (B7 owns it). Idempotent — Bevy's
/// `register_type` ignores duplicates, so calling it from more than one plugin
/// `build()` is harmless.
///
/// `AudioNode`, `NodeKind` (the by-value dispatch key still survives), and the
/// runtime handle/cache types are deliberately *not* registered here:
/// `AudioNode` wraps a foreign non-`Reflect` `NodeId`. `NodeKind` is `Reflect`
/// and is registered. The deliberately-non-`Reflect` set (AudioEmitter,
/// PluginEmitter, TrackClipReader*, …) is owned by other duties and skipped.
pub fn register_audio_node_types(app: &mut App) {
    use crate::core::ecs::*;

    app.register_type::<NodeKind>()
        // Scalar params (all carry `Default`).
        .register_type::<Volume>()
        .register_type::<Pan>()
        .register_type::<Mute>()
        .register_type::<Frequency>()
        .register_type::<FilterQ>()
        .register_type::<GainDb>()
        .register_type::<WetMix>()
        .register_type::<Feedback>()
        .register_type::<DelayTime>()
        .register_type::<ModRate>()
        .register_type::<ModDepth>()
        .register_type::<ThresholdDb>()
        .register_type::<CompressorRatio>()
        .register_type::<Attack>()
        .register_type::<Release>()
        .register_type::<CeilingDb>()
        .register_type::<Drive>()
        .register_type::<ReverbRoomSize>()
        .register_type::<ReverbDamping>()
        .register_type::<ReverbAlgo>()
        .register_type::<Azimuth>()
        .register_type::<Elevation>()
        .register_type::<SamplerSpeed>()
        .register_type::<SamplerLooping>()
        .register_type::<ModParam>()
        // Construction-only authored data.
        .register_type::<StereoChannels>()
        .register_type::<MaxDelay>()
        .register_type::<FilterMode>()
        .register_type::<LfoShapeKind>()
        .register_type::<BeatSynced>()
        .register_type::<ReverbTime>()
        // Authoring markers. Only the markers a spawn system (`Added<T>`) or a
        // reconciler (`With<T>`) actually reads are kept: the 6 generic
        // `spawn_dsp_node` triggers, the LFO trigger, and the three type-guard
        // markers (Reverb / ConvolutionReverb / Sampler). The 8 never-filtered
        // markers (Eq, Ladder, Flanger, Phaser, Distortion, Limiter,
        // BrickwallLimiter, SpatialPanner) were inserted but never queried, so
        // they were deleted along with the central `insert_node_marker` match.
        .register_type::<CompressorNode>()
        .register_type::<GateNode>()
        .register_type::<FilterNode>()
        .register_type::<ReverbNode>()
        .register_type::<ConvolutionReverbNode>()
        .register_type::<DelayNode>()
        .register_type::<ChorusNode>()
        .register_type::<SamplerNode>()
        .register_type::<LfoNodeMarker>();
}

/// Bevy plugin: graph reconciliation pipeline.
///
/// Runs the four-phase reconcile cycle every `Update`: `Spawn` → `Params`
/// → `Despawn` → `Commit`. Other plugins hook into these sets to interleave
/// their work.
pub struct TuttiGraphPlugin;

impl Plugin for TuttiGraphPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<GraphDirty>()
            .init_resource::<NodeParamEpoch>()
            .configure_sets(
                Update,
                (
                    GraphReconcileSystems::Spawn,
                    GraphReconcileSystems::Params,
                    GraphReconcileSystems::Despawn,
                    GraphReconcileSystems::Commit,
                )
                    .chain(),
            );

        // Per-node param-epoch bumps. Driven by `Changed<T>` on the param
        // components themselves (not the reconcilers), so they fire even when a
        // reconciler doesn't, and can't drift out of sync with the reconciler
        // list. The dsp-family bump lives in `TuttiDspPlugin` (dsp-gated).
        app.add_systems(Update, bump_param_epoch_core);
        #[cfg(feature = "sampler")]
        app.add_systems(Update, param_epoch::bump_param_epoch_sampler);
        #[cfg(feature = "plugin")]
        app.add_systems(Update, param_epoch::bump_param_epoch_plugin);

        // Graph-node removal is handled by an `On<Remove, AudioNode>`
        // observer (fires at command-flush, reads the still-present NodeId).
        // Sidechain teardown is handled by an `On<Remove, SidechainOf>`
        // observer; only the *add* half stays a Spawn-set system.
        app.add_observer(reconcile_node_despawn)
            .add_observer(reconcile_sidechain_remove);

        app.add_systems(
            Update,
            (
                reconcile_params.in_set(GraphReconcileSystems::Params),
                commit_graph.in_set(GraphReconcileSystems::Commit),
                reconcile_sidechain_links.in_set(GraphReconcileSystems::Spawn),
                reconcile_audio_routing.in_set(GraphReconcileSystems::Spawn),
            )
                .run_if(engine_ready),
        );

        #[cfg(feature = "sampler")]
        {
            app.init_resource::<WaveImportQueue>().add_systems(
                Update,
                (
                    reconcile_sampler_params.in_set(GraphReconcileSystems::Params),
                    promote_pending_samplers
                        .after(poll_wave_imports)
                        .in_set(GraphReconcileSystems::Spawn),
                )
                    .run_if(engine_ready),
            );
            // `poll_wave_imports` only touches `WaveImportQueue` + `Assets`, not
            // an engine resource, so it stays ungated.
            app.add_systems(Update, poll_wave_imports);
        }

        #[cfg(feature = "convolution")]
        {
            // `start_convolver_loads` only uses `AssetServer` (not an engine
            // resource); `promote_pending_convolvers` needs the graph.
            app.add_systems(Update, start_convolver_loads);
            app.add_systems(
                Update,
                promote_pending_convolvers
                    .after(start_convolver_loads)
                    .in_set(GraphReconcileSystems::Spawn)
                    .run_if(engine_ready),
            );
        }

        // `reconcile_plugin_params` writes through `PluginEmitter`, holds no
        // engine resource, so it stays ungated.
        #[cfg(feature = "plugin")]
        app.add_systems(
            Update,
            reconcile_plugin_params.in_set(GraphReconcileSystems::Params),
        );

        #[cfg(feature = "midi")]
        app.add_systems(Update, tick_scheduled_midi.run_if(engine_ready));
    }
}
