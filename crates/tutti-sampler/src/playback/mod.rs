//! Playback domain — everything that spawns, reconciles, or wraps a
//! `SamplerUnit` playback node.
//!
//! - [`trigger`] — `PlayAudio` one-shot trigger + `AudioVolume`/cleanup.
//! - [`pending_load`] — deferred wave-load → entity-as-node promotion.
//! - [`reconcile`] — `SamplerUnit` param/volume reconcilers + param-epoch bump.
//! - [`time_stretch`] — `TimeStretch` component → lock-free control sync.
//! - [`track_clip_reader`] — per-track multi-clip reader unit.
//! - [`units`] — the DSP leaves (`SamplerUnit`, `StreamingSamplerUnit`, …).
//!
//! [`TuttiPlaybackPlugin`] registers the whole domain in one place.

use bevy_app::{App, Plugin, Update};
use bevy_asset::AssetApp;
use bevy_ecs::schedule::IntoScheduleConfigs;

use tutti_core::ecs::{engine_ready, GraphReconcileSystems};
use tutti_core::WaveAsset;

mod loop_crossfade;
pub mod pending_load;
pub mod reconcile;
pub mod sampler_unit;
pub mod streaming_sampler;
pub mod time_stretch;
pub mod time_stretch_dsp;
pub mod track_clip_reader;
pub mod trigger;
pub mod wave_loader;

pub use pending_load::{
    poll_wave_imports, promote_pending_samplers, PendingSamplerLoad, WaveImportQueue,
};
pub use reconcile::{bump_param_epoch_sampler, reconcile_sampler_params, reconcile_sampler_volume};
pub use time_stretch::{time_stretch_sync_system, TimeStretch, TimeStretchControl};
pub use track_clip_reader::{
    ClipCommand, ClipSpec, SlotId, TrackClipReaderHandle, TrackClipReaderNode, TrackClipReaderRef,
    TrackClipReaderUnit,
};
pub use trigger::{
    audio_cleanup_system, audio_parameter_sync_system, audio_playback_system, AudioEmitter,
    AudioPlaybackState, AudioVolume, DespawnOnFinish, PlayAudio,
};
pub use sampler_unit::SamplerUnit;
pub use streaming_sampler::StreamingSamplerUnit;
pub use wave_loader::{WaveAssetLoader, WaveAssetLoaderError};

/// Bevy plugin: the whole playback domain — trigger → sampler → cleanup,
/// deferred wave-load promotion, `SamplerUnit` param reconcilers, the sampler
/// param-epoch bump, and time-stretch control sync.
///
/// Requires the core graph plugin ([`tutti_core::ecs::TuttiGraphPlugin`]) to
/// have configured `GraphReconcileSystems` first.
pub struct TuttiPlaybackPlugin;

impl Plugin for TuttiPlaybackPlugin {
    fn build(&self, app: &mut App) {
        app.init_asset::<WaveAsset>()
            .register_asset_loader(wave_loader::WaveAssetLoader);

        app.register_type::<AudioPlaybackState>()
            .register_type::<DespawnOnFinish>()
            .register_type::<AudioVolume>()
            .register_type::<PlayAudio>()
            .register_type::<TimeStretch>();

        // Trigger lifecycle. These stage graph edits + set GraphDirty; anchor
        // the chain before the Commit phase so the once-per-frame `commit_graph`
        // coalesces them. spatial.rs hangs its sync system between playback and
        // cleanup via .after/.before, so that relative order is preserved.
        app.add_systems(
            Update,
            (
                trigger::audio_playback_system,
                trigger::audio_parameter_sync_system,
                trigger::audio_cleanup_system,
            )
                .chain()
                .run_if(engine_ready)
                .before(GraphReconcileSystems::Commit),
        );

        // Time-stretch control sync — after playback so `TimeStretchControl`
        // exists before this writes to it.
        app.add_systems(
            Update,
            time_stretch::time_stretch_sync_system.after(trigger::audio_playback_system),
        );

        // Sampler param-epoch bump (core bump is added by the core plugin).
        app.add_systems(Update, reconcile::bump_param_epoch_sampler);

        // Param reconcilers + deferred-load promotion in the shared reconcile
        // schedule. `poll_wave_imports` only touches `WaveImportQueue` + assets
        // (no engine resource), so it stays ungated.
        app.init_resource::<WaveImportQueue>()
            .add_systems(
                Update,
                (
                    reconcile::reconcile_sampler_volume.in_set(GraphReconcileSystems::Params),
                    reconcile::reconcile_sampler_params.in_set(GraphReconcileSystems::Params),
                    pending_load::promote_pending_samplers
                        .after(pending_load::poll_wave_imports)
                        .in_set(GraphReconcileSystems::Spawn),
                )
                    .run_if(engine_ready),
            )
            .add_systems(Update, pending_load::poll_wave_imports);
    }
}
