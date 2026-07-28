//! Voice playback — the DSP units that turn a wave into audio, plus the shared
//! kernels they read through.
//!
//! - [`memory_source`] — in-memory playback ([`MemorySource`]).
//! - [`disk_voice`] — disk-streaming playback, fed by the butler thread.
//! - [`types`] — the voice vocabulary: `Voice`, `VoiceSource`, `Playback`.
//! - [`slot`] — a voice plus its stretch filter, and the per-sample read.
//! - [`command`] — the ECS → audio-thread protocol and its handle.
//! - [`pool`] — per-track multi-voice mixer over both tiers.
//! - [`node`] — one voice as a standalone graph node.
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
pub mod disk_voice;
// The voice pool, one file per duty. Each holds Bevy-free DSP; the ECS pieces
// are gated inside `pool`.
pub mod command;
pub mod node;
pub mod pool;
pub mod slot;
pub mod types;
// Tests only — they exercise the five modules above in combination and reach
// private state a sibling module could not see.
mod voice_pool;

// The sampler's Bevy asset loader. The DAW-facing ECS binding (param
// write-through, deferred load) lives app-side in
// `dawai_model::engine_bind::sampler`, with the `Volume`/`Mute` components it
// reads.
#[cfg(feature = "bevy")]
pub mod wave_loader;

// Bevy-free reader value types + DSP unit.
pub use disk_voice::{DiskSource, DiskVoice, DiskVoiceConfig};
pub use memory_source::{LoopSetting, MemorySource, MemorySourceConfig, VoiceWindow};
// Re-exported flat, so `voice::Voice` and `tutti_sampler::Voice` keep working —
// the split is an internal reorganisation, not an API change.
pub use command::{VoiceCommand, VoicePoolHandle};
pub use node::VoiceNode;
pub use pool::VoicePool;
#[cfg(feature = "bevy")]
pub use pool::{VoicePoolNode, VoicePoolRef};
pub use types::{Direction, Playback, SlotId, Voice, VoiceSource};
#[cfg(feature = "bevy")]
pub use wave_loader::{WaveAssetLoader, WaveAssetLoaderError};

/// Bevy plugin for voice playback: registers the wave asset loader.
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
