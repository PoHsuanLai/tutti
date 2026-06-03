//! Bevy ECS integration for the sampler subsystem.
//!
//! This is the sampler-domain half of bevy-tutti's audio-graph reconciler,
//! folded into the leaf crate so the Components / Systems / Plugins live next
//! to the audio logic they drive. The generic reconcile hub
//! (`GraphReconcileSystems`, `SpawnAudioNode`, `engine_ready`, `GraphDirty`,
//! `reconcile_params`, `commit_graph`, …) lives in [`tutti_core::ecs`]; this
//! module layers the sampler-specific pieces on top:
//!
//! - [`playback`] — `PlayAudio` trigger → `SamplerUnit` + cleanup.
//! - [`recording`] — `StartRecording` / `StopRecording`.
//! - [`audio_input`] — hardware-input device control + peak mirror.
//! - [`time_stretch`] — lock-free pitch/duration control.
//! - [`auditioner`] — browser file preview.
//! - [`track_clip_reader`] — per-track multi-clip reader unit + ECS glue.
//! - [`content_bounds`] — project content-length resource.
//! - [`pending_load`] — deferred wave-load → entity-as-node promotion.
//! - [`reconcile`] — sampler param reconcilers + param-epoch bump.

use std::sync::Arc;

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use bevy_ecs::schedule::IntoScheduleConfigs;

use tutti_core::ecs::{engine_ready, GraphReconcileSystems};

use crate::Sampler;

pub mod audio_input;
pub mod auditioner;
pub mod content_bounds;
pub mod pending_load;
pub mod playback;
pub mod reconcile;
pub mod recording;
pub mod time_stretch;
pub mod track_clip_reader;

pub use audio_input::{
    audio_input_control_system, audio_input_init_system, audio_input_sync_system,
    AudioInputDeviceInfo, AudioInputState, DisableAudioInput, EnableAudioInput,
    TuttiAudioInputPlugin,
};
pub use auditioner::{
    init_auditioner, AuditionerNode, AuditionerRes, PreviewFile, StopPreview, TuttiAuditionerPlugin,
};
pub use content_bounds::ContentBounds;
pub use pending_load::{
    poll_wave_imports, promote_pending_samplers, PendingSamplerLoad, WaveImportQueue,
};
pub use playback::{
    audio_cleanup_system, audio_parameter_sync_system, audio_playback_system, AudioEmitter,
    AudioPlaybackState, AudioVolume, DespawnOnFinish, PlayAudio, TuttiPlaybackPlugin,
};
pub use reconcile::{
    bump_param_epoch_sampler, reconcile_sampler_params, reconcile_sampler_volume,
};
pub use recording::{
    recording_start_system, recording_stop_system, RecordingActive, RecordingResult,
    StartRecording, StopRecording, TuttiRecordingPlugin,
};
pub use time_stretch::{
    time_stretch_sync_system, TimeStretch, TimeStretchControl, TuttiTimeStretchPlugin,
};
pub use track_clip_reader::{
    ClipCommand, ClipSpec, SlotId, TrackClipReaderHandle, TrackClipReaderNode, TrackClipReaderRef,
    TrackClipReaderUnit,
};

/// Sampler subsystem (disk streaming, clip playback, capture) as a Bevy
/// resource. Holds the shared `Arc<Sampler>` built by the engine; the
/// `TuttiSamplerPlugin` does **not** build the sampler — the host inserts this
/// resource from the engine bundle.
#[derive(Resource, Clone)]
pub struct SamplerRes(pub Arc<Sampler>);

impl std::ops::Deref for SamplerRes {
    type Target = Sampler;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Bevy plugin: the whole sampler ECS surface.
///
/// Composes the per-duty sub-plugins (playback, recording, audio-input,
/// time-stretch, auditioner) and adds the sampler reconcilers + pending-load
/// promotion + param-epoch bump into the shared `GraphReconcileSystems`
/// schedule owned by [`tutti_core::ecs`].
///
/// Mirrors what bevy-tutti's `TuttiGraphPlugin` sampler block + the individual
/// sub-plugins did, now in one place. Requires the core graph plugin
/// ([`tutti_core::ecs::TuttiGraphPlugin`]) to have configured
/// `GraphReconcileSystems` first.
pub struct TuttiSamplerPlugin;

impl Plugin for TuttiSamplerPlugin {
    fn build(&self, app: &mut App) {
        // Register the sampler authoring marker so the type registry knows it.
        app.register_type::<tutti_core::ecs::SamplerNode>();

        app.add_plugins((
            TuttiPlaybackPlugin,
            TuttiRecordingPlugin,
            TuttiAudioInputPlugin,
            TuttiTimeStretchPlugin,
            TuttiAuditionerPlugin,
        ));

        // The `ContentBounds` resource lives here (the sampler domain owns the
        // type), but it's populated downstream from ECS clip placements by the
        // host — the doc/ECS is the source of truth for project length.
        app.init_resource::<ContentBounds>();
        app.register_type::<ContentBounds>();

        // Sampler param-epoch bump (core bump is added by the core plugin).
        app.add_systems(Update, bump_param_epoch_sampler);

        app.init_resource::<WaveImportQueue>().add_systems(
            Update,
            (
                reconcile_sampler_volume.in_set(GraphReconcileSystems::Params),
                reconcile_sampler_params.in_set(GraphReconcileSystems::Params),
                promote_pending_samplers
                    .after(poll_wave_imports)
                    .in_set(GraphReconcileSystems::Spawn),
            )
                .run_if(engine_ready),
        );
        // `poll_wave_imports` only touches `WaveImportQueue` + `Assets`, not an
        // engine resource, so it stays ungated.
        app.add_systems(Update, poll_wave_imports);
    }
}
