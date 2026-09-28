//! Clip playback and sample assets (feature `sampler`): the `.wav` loader, the
//! disk-streaming engine handle and voice spawning.
//!
//! The adapter for `tutti-sampler`. [`DiskStreamerRes`] holds the engine's
//! [`DiskStreamer`], which owns the butler thread that streams clips from disk;
//! [`TuttiPlaybackPlugin`] registers the [`WaveAsset`] loader; and [`voice`]
//! spawns clip voices as graph nodes.

use bevy_app::{App, Plugin};
use bevy_asset::AssetApp;
use bevy_ecs::prelude::*;

use tutti_sampler::DiskStreamer;

// What `SpawnVoice` / `memory_voice` take and what the `.wav` loader hands out,
// re-exported so a Bevy host playing a clip needs no direct `tutti-io`
// dependency. (`sampler` enables `tutti-io/bevy`, so `WaveAsset` always exists
// here.)
pub use tutti_io::{Wave, WaveAsset};

pub mod voice;
pub mod wave_loader;

pub use voice::{memory_voice, voice_width, InsertVoice, SamplerVoice, SpawnVoice, VoiceCommands};
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
pub struct DiskStreamerRes(
    /// The engine handle. Reachable through this crate's `Deref` too, which is
    /// how the two ports above are normally called.
    pub DiskStreamer,
);

impl std::ops::Deref for DiskStreamerRes {
    type Target = DiskStreamer;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Registers the [`WaveAsset`] loader, so a host can `asset_server.load()` a
/// `.wav` and hand the result to a voice.
///
/// Added by [`TuttiPlugin`](crate::TuttiPlugin) with the `sampler` feature.
/// Needs an `AssetServer` (`bevy_asset::AssetPlugin`) already in the app.
#[derive(Debug)]
pub struct TuttiPlaybackPlugin;

impl Plugin for TuttiPlaybackPlugin {
    fn build(&self, app: &mut App) {
        app.init_asset::<WaveAsset>()
            .register_asset_loader(WaveAssetLoader);
    }
}
