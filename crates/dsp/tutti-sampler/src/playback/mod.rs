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

#[cfg(feature = "bevy")]
use bevy_app::{App, Plugin, Update};
#[cfg(feature = "bevy")]
use bevy_asset::AssetApp;
#[cfg(feature = "bevy")]
use bevy_ecs::schedule::IntoScheduleConfigs;

#[cfg(feature = "bevy")]
use tutti_core::ecs::{engine_ready, GraphReconcileSystems};
#[cfg(feature = "bevy")]
use tutti_core::WaveAsset;

// Bevy-free DSP leaves — always compiled.
mod loop_crossfade;
// The cold-path control trait shared by both clip-playback backends.
pub mod clip_reader;
// Shared zero-alloc interpolation kernel (one cubic Hermite for both units).
pub mod interp;
pub mod sampler_unit;
// Disk streaming — the unit is Bevy-free; it's fed by the (Bevy-free) butler
// engine, which a non-Bevy host drives via `Sampler` / `Auditioner`.
pub mod streaming_sampler;
// Live-input monitoring — the mic twin of the streaming unit. Device-free
// (holds only a ring consumer); the device layer in `bevy-tutti` fills it.
pub mod mic_monitor;
// `time_stretch` / `track_clip_reader` hold Bevy-free DSP (their ECS pieces are
// gated inside each module).
pub mod time_stretch;
pub mod track_clip_reader;

// Pure Bevy-glue modules.
#[cfg(feature = "bevy")]
pub mod node;
#[cfg(feature = "bevy")]
pub mod pending_load;
#[cfg(feature = "bevy")]
pub mod reconcile;
#[cfg(feature = "bevy")]
pub mod trigger;
#[cfg(feature = "bevy")]
pub mod wave_loader;

#[cfg(feature = "bevy")]
pub use pending_load::{
    poll_wave_imports, promote_pending_samplers, PendingSamplerLoad, WaveImportQueue,
};
#[cfg(feature = "bevy")]
pub use reconcile::{bump_param_epoch_sampler, reconcile_sampler_params, reconcile_sampler_volume};
#[cfg(feature = "bevy")]
pub use time_stretch::{time_stretch_sync_system, TimeStretch, TimeStretchControl};
// Bevy-free reader value types + DSP unit.
pub use clip_reader::ClipReader;
pub use mic_monitor::{share_mic_ring, MicMonitorNode, MicRing};
#[cfg(feature = "bevy")]
pub use node::{SamplerLooping, SamplerNode, SamplerSpeed};
pub use sampler_unit::{LoopSetting, SamplerUnit, SamplerUnitConfig, TransportPlacement};
pub use streaming_sampler::{StreamingClipConfig, StreamingClipReader, StreamingSamplerUnit};
pub use track_clip_reader::{
    ClipCommand, ClipSpec, Direction, PendingPlayback, Playback, SlotId, TrackClipReaderHandle,
    TrackClipReaderUnit, Voice, VoiceNode, VoiceSource,
};
#[cfg(feature = "bevy")]
pub use track_clip_reader::{TrackClipReaderNode, TrackClipReaderRef};
#[cfg(feature = "bevy")]
pub use trigger::{
    audio_cleanup_system, audio_parameter_sync_system, audio_playback_system, AudioEmitter,
    AudioPlaybackState, AudioVolume, DespawnOnFinish, PlayAudio,
};
#[cfg(feature = "bevy")]
pub use wave_loader::{WaveAssetLoader, WaveAssetLoaderError};

/// Bevy plugin: the whole playback domain — trigger → sampler → cleanup,
/// deferred wave-load promotion, `SamplerUnit` param reconcilers, the sampler
/// param-epoch bump, and time-stretch control sync.
///
/// Requires the core graph plugin ([`tutti_core::ecs::GraphReconcilePlugin`]) to
/// have configured `GraphReconcileSystems` first.
#[cfg(feature = "bevy")]
#[derive(Debug)]
pub struct TuttiPlaybackPlugin;

#[cfg(feature = "bevy")]
impl Plugin for TuttiPlaybackPlugin {
    fn build(&self, app: &mut App) {
        app.init_asset::<WaveAsset>()
            .register_asset_loader(wave_loader::WaveAssetLoader);

        app.register_type::<AudioPlaybackState>()
            .register_type::<DespawnOnFinish>()
            .register_type::<AudioVolume>()
            .register_type::<PlayAudio>()
            .register_type::<TimeStretch>()
            .register_type::<node::SamplerNode>()
            .register_type::<node::SamplerSpeed>()
            .register_type::<node::SamplerLooping>();

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
