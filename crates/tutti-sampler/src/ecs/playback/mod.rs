//! Sample playback: trigger → SamplerUnit → cleanup.
//!
//! Three sub-concepts:
//! - [`emitter`] — the `PlayAudio` trigger and its spawn system.
//! - [`volume`] — `AudioVolume` parameter sync.
//! - [`cleanup`] — finished-sample detection, graph removal, optional despawn.

use bevy_app::{App, Plugin, Update};
use bevy_asset::AssetApp;
use bevy_ecs::schedule::IntoScheduleConfigs;

use tutti_core::ecs::{engine_ready, GraphReconcileSystems};
use tutti_core::WaveAsset;

use crate::loader::{StreamingSampleLoader, WaveAssetLoader};
use crate::StreamingSample;

mod cleanup;
mod emitter;
mod volume;

pub use cleanup::{audio_cleanup_system, DespawnOnFinish};
pub use emitter::{audio_playback_system, AudioEmitter, AudioPlaybackState, PlayAudio};
pub use volume::{audio_parameter_sync_system, AudioVolume};

/// Bevy plugin: sample playback (trigger → sampler → cleanup).
pub struct TuttiPlaybackPlugin;

impl Plugin for TuttiPlaybackPlugin {
    fn build(&self, app: &mut App) {
        app.init_asset::<WaveAsset>()
            .register_asset_loader(WaveAssetLoader);

        app.register_type::<AudioPlaybackState>()
            .register_type::<DespawnOnFinish>()
            .register_type::<AudioVolume>()
            .register_type::<PlayAudio>();

        // These stage graph edits + set GraphDirty; anchor the whole chain
        // before the Commit phase so the once-per-frame `commit_graph`
        // coalesces them (they no longer commit inline). spatial.rs hangs
        // its sync system between playback and cleanup via .after/.before,
        // so that relative order is preserved by the chain.
        app.init_asset::<StreamingSample>()
            .register_asset_loader(StreamingSampleLoader)
            .add_systems(
                Update,
                (
                    audio_playback_system,
                    audio_parameter_sync_system,
                    audio_cleanup_system,
                )
                    .chain()
                    .run_if(engine_ready)
                    .before(GraphReconcileSystems::Commit),
            );
    }
}
