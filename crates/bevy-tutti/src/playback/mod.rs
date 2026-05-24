//! Sample playback: trigger → SamplerUnit → cleanup.
//!
//! Three sub-concepts:
//! - [`emitter`] — the `PlayAudio` trigger and its spawn system.
//! - [`volume`] — `AudioVolume` parameter sync.
//! - [`cleanup`] — finished-sample detection, graph removal, optional despawn.

use bevy_app::{App, Plugin};
#[cfg(feature = "sampler")]
use bevy_app::Update;
use bevy_asset::AssetApp;
#[cfg(feature = "sampler")]
use bevy_ecs::prelude::*;

use crate::loader::TuttiLoader;
use tutti::core::WaveAsset;
#[cfg(feature = "sampler")]
use crate::loader::TuttiStreamingLoader;
#[cfg(feature = "sampler")]
use tutti::sampler::StreamingSample;

mod cleanup;
mod emitter;
mod volume;

pub use cleanup::DespawnOnFinish;
#[cfg(feature = "sampler")]
pub use cleanup::audio_cleanup_system;
pub use emitter::{AudioEmitter, AudioPlaybackState, PlayAudio};
#[cfg(feature = "sampler")]
pub use emitter::audio_playback_system;
pub use volume::AudioVolume;
#[cfg(feature = "sampler")]
pub use volume::audio_parameter_sync_system;

/// Bevy plugin: sample playback (trigger → sampler → cleanup).
pub struct TuttiPlaybackPlugin;

impl Plugin for TuttiPlaybackPlugin {
    fn build(&self, app: &mut App) {
        app.init_asset::<WaveAsset>()
            .register_asset_loader(TuttiLoader::<WaveAsset>::default());

        app.register_type::<AudioPlaybackState>()
            .register_type::<DespawnOnFinish>()
            .register_type::<AudioVolume>()
            .register_type::<PlayAudio>();

        #[cfg(feature = "sampler")]
        {
            app.init_asset::<StreamingSample>()
                .register_asset_loader(TuttiStreamingLoader::<StreamingSample>::default())
                .add_systems(
                    Update,
                    (
                        audio_playback_system,
                        audio_parameter_sync_system,
                        audio_cleanup_system,
                    )
                        .chain(),
                );
        }
    }
}
