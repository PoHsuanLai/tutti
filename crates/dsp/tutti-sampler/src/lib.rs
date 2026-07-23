//! Sample playback, disk streaming, and time-stretching for the Tutti audio
//! engine.
//!
//! # Bevy-native API
//!
//! The integration follows the `bevy_audio` shape: each duty is a crate-root
//! module owning its Components / Systems / Plugin, composed by the single
//! [`TuttiSamplerPlugin`]. Drive it by spawning entities and writing messages,
//! not through handle methods:
//!
//! ```ignore
//! // One-shot playback: spawn an entity carrying the trigger.
//! commands.spawn(PlayAudio { source: asset_server.load("clip.wav"), ..default() });
//! ```
//!
//! The [`Sampler`] resource is the streaming-engine handle (the butler thread).
//! The engine builds it once with [`Sampler::new`] and inserts it; systems read
//! it as `Res<Sampler>` and drive streaming through the [`commands()`](Sampler::commands)
//! WRITE port and the [`status()`](Sampler::status) READ port.
//!
//! # Crate layout
//!
//! Bevy duties are crate-root modules ([`playback`], [`time_stretch`],
//! [`pending_load`], [`reconcile`], [`track_clip_reader`]). Value types and DSP
//! internals live in purpose-named namespaces:
//!
//! - [`AudioIn`] / [`AudioOut`] / [`pump`] — the engine's I/O edge vocabulary,
//!   re-exported from [`tutti_types::io`]. Recording is a pump from one to the
//!   other.
//! - [`capture`] — the write side's live impl: [`WavSink`](capture::WavSink),
//!   an [`AudioOut`]
//! - [`stretch`] — time-stretch / pitch-shift DSP unit
//!
//! # Bevy-free use (`--no-default-features`)
//!
//! The whole engine is Bevy-free; only the ECS drivers (Plugins, trigger
//! messages, reconcile systems) are behind the `bevy` feature. A non-Bevy host
//! builds a [`Sampler`] and drives it through plain methods:
//!
//! - In-memory playback: construct a [`SamplerUnit`] / [`TrackClipReaderUnit`]
//!   and add it to a [`tutti_core::graph::AudioGraph`].
//! - **Disk streaming**: [`Sampler::new`] spawns the butler thread;
//!   [`commands()`](Sampler::commands) issues stream/seek/loop ops and
//!   [`status()`](Sampler::status) constructs a [`StreamingClipReader`] to wire
//!   into your graph.
//!
//! ```no_run
//! use std::sync::Arc;
//! use tutti_sampler::{SamplerUnit, stretch};
//! use tutti_core::Wave;
//!
//! let wave = Arc::new(Wave::with_capacity(1, 44_100.0, 0));
//! let unit = SamplerUnit::new(wave);
//! // The stretcher is a pure frame-in → frame-out filter: it owns no source.
//! // The caller ticks `unit` and feeds each frame into `stretched`.
//! let stretched = stretch::Unit::new(44_100.0);
//! # let _ = (unit, stretched);
//! ```

pub mod error;
pub use error::{Error, Result};

#[macro_use]
mod macros;

mod node_id;

// The engine's I/O edge vocabulary lives in `tutti-types` (the root leaf) so
// every subsystem shares one definition. Re-exported here for back-compat so
// `tutti_sampler::{AudioIn, AudioOut, pump}` paths keep resolving.
pub use tutti_core::io;
pub use tutti_core::io::{pump, AudioIn, AudioOut};

// Each domain is a self-contained module owning its audio engine + (under the
// `bevy` feature) its Components / Systems / Plugin. The disk-streaming engine —
// the butler thread, the `Sampler` handle, `StreamingSamplerUnit` — is Bevy-free:
// a non-Bevy host builds a `Sampler` with `Sampler::new(..)` and drives streaming
// through its `commands()`/`status()` ports directly. Only the ECS drivers
// (Plugins, trigger Messages, reconcile systems) that turn those into entity-spawn
// workflows are gated.
pub mod playback;

// Bevy-free DSP leaves + value types from `playback` — usable for direct
// FunDSP-graph integration without the ECS layer.
pub use butler::{LruCache, StreamPin, WavSink};
pub use playback::{
    ClipCommand, ClipReader, ClipSpec, Direction, LoopSetting, PendingPlayback, Playback,
    SamplerUnit, SamplerUnitConfig, SlotId, StreamingClipConfig, StreamingClipReader,
    StreamingSamplerUnit, TrackClipReaderHandle, TrackClipReaderUnit, TransportPlacement, Voice,
    VoiceNode, VoiceSource,
};
// Bevy ECS surface of `playback`.
#[cfg(feature = "bevy")]
pub use playback::{
    audio_cleanup_system, audio_parameter_sync_system, audio_playback_system,
    bump_param_epoch_sampler, poll_wave_imports, promote_pending_samplers,
    reconcile_sampler_params, reconcile_sampler_volume, time_stretch_sync_system, AudioEmitter,
    AudioPlaybackState, AudioVolume, DespawnOnFinish, PendingSamplerLoad, PlayAudio,
    SamplerLooping, SamplerNode, SamplerSpeed, TimeStretch, TimeStretchControl,
    TrackClipReaderNode, TrackClipReaderRef, TuttiPlaybackPlugin, WaveAssetLoader,
    WaveAssetLoaderError, WaveImportQueue,
};

// The async disk-streaming engine (butler thread + the `Sampler` handle). All
// Bevy-free — a non-Bevy host drives it directly via `Sampler::new` / the
// `ButlerThread` API. The `Resource` derive on `Sampler` is `bevy`-gated inside.
pub(crate) mod butler;

mod sampler;
pub use sampler::{Sampler, SamplerConfig};

mod ports;
pub use ports::{Command, Commands, Source, Status};

#[cfg(feature = "bevy")]
use bevy_ecs::prelude::Resource;

/// Transient handed off by `build_into`, part of the umbrella's uniform
/// `PendingX` init handshake: the builder inserts the freshly-built [`Sampler`]
/// and [`TuttiSamplerPlugin`]'s `build()` claims it into a real resource.
#[cfg(feature = "bevy")]
#[derive(Resource)]
pub struct PendingSampler(pub Option<Sampler>);

/// Bevy plugin: the whole sampler ECS surface.
///
/// Composes the per-duty sub-plugins (playback, time-stretch) and adds the
/// sampler reconcilers + pending-load promotion + param-epoch bump into the
/// shared `GraphReconcileSystems` schedule owned by [`tutti_core::graph`].
/// Requires the core graph plugin ([`tutti_core::graph::GraphReconcilePlugin`])
/// to have configured `GraphReconcileSystems` first.
#[cfg(feature = "bevy")]
pub struct TuttiSamplerPlugin;

#[cfg(feature = "bevy")]
impl bevy_app::Plugin for TuttiSamplerPlugin {
    fn build(&self, app: &mut bevy_app::App) {
        // Each domain is a self-contained plugin; this just composes them.
        // (`TuttiPlaybackPlugin` owns the trigger lifecycle, deferred-load
        // promotion, `SamplerUnit` reconcilers, param-epoch bump, and
        // time-stretch sync.)
        app.add_plugins(playback::TuttiPlaybackPlugin);

        // Claim the sampler out of the transient `build_into` inserted
        // (synchronous, during plugin build) — the umbrella `PendingX` handshake.
        if let Some(PendingSampler(Some(sampler))) =
            app.world_mut().remove_resource::<PendingSampler>()
        {
            app.insert_resource(sampler);
        }
    }
}

/// The write side's live impl: [`WavSink`], an [`AudioOut`] that streams stereo
/// frames to a WAV file, plus its [`CaptureFormat`](capture::CaptureFormat).
///
/// The record-mic→WAV flow is an explicit [`AudioIn`] → [`AudioOut`] pump
/// driving this sink, lived out by bevy-tutti's `Recorder` (a `MicSource`
/// pumped into a `WavSink` on a background thread).
pub mod capture {
    pub use crate::butler::{CaptureFormat, WavSink};
}

/// Time-stretching and pitch-shifting DSP unit.
pub mod stretch {
    pub use crate::playback::time_stretch::{Algorithm, FftSize, Params, Unit};
}
