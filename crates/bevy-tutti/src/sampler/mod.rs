//! The sampler's asset layer: the `.wav` loader and the streaming-engine
//! handle.

use bevy_app::{App, Plugin};
use bevy_asset::AssetApp;
use bevy_ecs::prelude::*;

use tutti_core::WaveAsset;
use tutti_sampler::DiskStreamer;

pub mod wave_loader;

pub use wave_loader::{WaveAssetLoader, WaveAssetLoaderError};

/// The sampler streaming engine. Owns the butler thread that drives all disk
/// I/O; built by [`build_into`](crate::engine::build_into).
#[derive(Resource)]
pub struct SamplerRes(pub DiskStreamer);

impl std::ops::Deref for SamplerRes {
    type Target = DiskStreamer;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Registers the wave asset loader.
#[derive(Debug)]
pub struct TuttiPlaybackPlugin;

impl Plugin for TuttiPlaybackPlugin {
    fn build(&self, app: &mut App) {
        app.init_asset::<WaveAsset>()
            .register_asset_loader(WaveAssetLoader);
    }
}
