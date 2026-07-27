//! Clip playback — the DSP units that turn a wave into audio, plus the shared
//! kernels they read through.
//!
//! - [`memory_source`] — in-memory playback ([`MemorySource`]).
//! - [`streaming_sampler`] — disk-streaming playback, fed by the butler thread.
//! - [`track_clip_reader`] — per-track multi-clip mixer over both tiers.
//! - [`interp`] — the interpolation kernel and the transport-placement gate.

#[cfg(feature = "bevy")]
use bevy_app::{App, Plugin};
#[cfg(feature = "bevy")]
use bevy_asset::AssetApp;
#[cfg(feature = "bevy")]
use tutti_core::WaveAsset;

// Bevy-free DSP leaves — always compiled.
mod loop_crossfade;
// Shared zero-alloc interpolation kernel (one cubic Hermite for both units).
pub mod interp;
pub mod memory_source;
// Disk streaming — the unit is Bevy-free; it's fed by the (Bevy-free) butler
// engine, which a non-Bevy host drives via `DiskStreamer`.
pub mod streaming_sampler;
// `track_clip_reader` holds Bevy-free DSP (its ECS pieces are gated inside).
pub mod track_clip_reader;

// The sampler's Bevy asset loader. The DAW-facing ECS binding (param
// write-through, deferred load) lives app-side in
// `dawai_model::engine_bind::sampler`, with the `Volume`/`Mute` components it
// reads.
#[cfg(feature = "bevy")]
pub mod wave_loader;

// Bevy-free reader value types + DSP unit.
pub use memory_source::{LoopSetting, MemorySource, MemorySourceConfig, TransportPlacement};
pub use streaming_sampler::{DiskSource, StreamingClipConfig, StreamingClipReader};
pub use track_clip_reader::{
    ClipCommand, ClipSpec, Direction, PendingPlayback, Playback, SlotId, TrackClipReaderHandle,
    TrackClipReaderUnit, Voice, VoiceNode, VoiceSource,
};
#[cfg(feature = "bevy")]
pub use track_clip_reader::{TrackClipReaderNode, TrackClipReaderRef};
#[cfg(feature = "bevy")]
pub use wave_loader::{WaveAssetLoader, WaveAssetLoaderError};

/// Bevy plugin for clip playback: registers the wave asset loader.
#[cfg(feature = "bevy")]
#[derive(Debug)]
pub struct TuttiPlaybackPlugin;

#[cfg(feature = "bevy")]
impl Plugin for TuttiPlaybackPlugin {
    fn build(&self, app: &mut App) {
        app.init_asset::<WaveAsset>()
            .register_asset_loader(wave_loader::WaveAssetLoader);

        // SamplerNode/Speed/Looping are registered app-side (engine_bind::sampler).

        // The sampler param reconcilers, epoch bump, and deferred-load promotion
        // moved app-side (dawai_model::engine_bind::sampler) with the DAW param
        // components they read.
    }
}
