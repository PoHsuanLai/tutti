//! Streaming and sample assets: the `.wav` loader and the disk-streaming
//! engine handle.
//!
//! Named for what the engine calls these things. There is no `Sampler` type in
//! `tutti-sampler` — the crate's noun is [`DiskStreamer`], the handle that owns
//! the butler thread — so a `SamplerRes` here named a type that does not exist.

use bevy_app::{App, Plugin};
use bevy_asset::AssetApp;
use bevy_ecs::prelude::*;

use tutti_core::WaveAsset;
use tutti_sampler::DiskStreamer;

pub mod voice;
pub mod wave_loader;

pub use voice::{memory_voice, voice_width, InsertVoice, SamplerVoice, SpawnVoice};
pub use wave_loader::{WaveAssetLoader, WaveAssetLoaderError};

/// The disk-streaming engine. Owns the butler thread that drives all disk I/O;
/// built by [`build_into`](crate::engine::build_into).
///
/// The two ports onto it are the engine's, reached through the `Deref`:
/// [`commands()`](DiskStreamer::commands) issues stream/seek/loop operations and
/// [`status()`](DiskStreamer::status) builds a `DiskVoice` to wire into the
/// graph. Neither is wrapped in ECS vocabulary — both speak in channel indices
/// and `Timeline` placements, which is clip-scheduling policy a host owns.
#[derive(Resource)]
pub struct DiskStreamerRes(pub DiskStreamer);

impl std::ops::Deref for DiskStreamerRes {
    type Target = DiskStreamer;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Registers the [`WaveAsset`] loader, so a host can `asset_server.load()` a
/// `.wav` and hand the result to a voice.
#[derive(Debug)]
pub struct TuttiPlaybackPlugin;

impl Plugin for TuttiPlaybackPlugin {
    fn build(&self, app: &mut App) {
        app.init_asset::<WaveAsset>()
            .register_asset_loader(WaveAssetLoader);
    }
}
