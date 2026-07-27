//! Sample playback, disk streaming, and time-stretching for the Tutti audio
//! engine.
//!
//! # Two playback tiers, one vocabulary
//!
//! A voice plays either from memory ([`MemorySource`]) or streamed from disk
//! ([`DiskVoice`], fed by the butler thread). The tier is the
//! caller's choice — the sampler never picks one on its own — and
//! [`VoicePool`] mixes both behind one command surface; where a verb
//! only makes sense on one tier, the `VoiceSource` match says so at the call
//! site instead of silently no-opping.
//!
//! Rates are typed to keep the tiers honest:
//! [`PlaybackRate`](tutti_core::PlaybackRate) is varispeed (couples pitch),
//! [`SrcRatio`](tutti_core::SrcRatio) is sample-rate conversion (derived, never
//! user intent), and [`StretchFactor`](tutti_core::StretchFactor) drives the
//! phase vocoder (pitch-independent). They compose only through
//! `PlaybackRate::read_rate`, and the varispeed range lives in one shared
//! bounded constructor that every user-input path goes through — it used to
//! live inside a single backend's setter, so the other tier silently accepted
//! out-of-range speeds.
//!
//! A voice bound to a transport derives its read position from the playhead
//! every frame rather than carrying a cursor, matching `tutti_core`'s transport:
//! one clock advances, everything else reads.
//!
//! The [`DiskStreamer`] handle owns the streaming engine. Build it once with
//! [`DiskStreamer::new`], then drive streaming through the
//! [`commands()`](DiskStreamer::commands) WRITE port and the
//! [`status()`](DiskStreamer::status) READ port.
//!
//! # Crate layout
//!
//! - [`voice`] — the two playback tiers, the per-track mixer, and the shared
//!   interpolation / placement kernels.
//! - [`live`] — mic in, WAV out.
//! - [`stretch`] — phase vocoder (pitch-independent stretch).
//! - [`AudioIn`] / [`AudioOut`] / [`pump`] — the engine's I/O edge vocabulary,
//!   re-exported from `tutti_types::io`.
//!
//! # Bevy-free use (`--no-default-features`)
//!
//! The engine is Bevy-free; only the ECS drivers are behind the `bevy` feature.
//! A non-Bevy host builds a [`DiskStreamer`] and drives it through plain methods:
//!
//! - In-memory playback: construct a [`MemorySource`] / [`VoicePool`]
//!   and add it to a fundsp `Net`.
//! - **Disk streaming**: [`DiskStreamer::new`] spawns the butler thread;
//!   [`commands()`](DiskStreamer::commands) issues stream/seek/loop ops and
//!   [`status()`](DiskStreamer::status) constructs a [`DiskVoice`] to wire
//!   into your graph.
//!
//! ```no_run
//! use std::sync::Arc;
//! use tutti_sampler::{MemorySource, stretch};
//! use tutti_core::Wave;
//!
//! let wave = Arc::new(Wave::with_capacity(1, 44_100.0, 0));
//! let unit = MemorySource::new(wave);
//! // The stretcher is a pure frame-in → frame-out filter: it owns no source.
//! // The caller ticks `unit` and feeds each frame into `stretched`.
//! let stretched = stretch::Unit::new(44_100.0);
//! # let _ = (unit, stretched);
//! ```

pub mod error;
pub use error::{Error, Result};

/// Widest frame the sampler reads, interpolates, or emits.
///
/// Deliberately equal to [`tutti_core::engine::MAX_ROOT_CHANNELS`] — the graph root's
/// own ceiling. **The two move together:** a voice wider than the root can render
/// is a voice nobody can hear, so there is no value in the sampler exceeding it,
/// and letting it do so would mean the truncation happened silently downstream
/// (at the root's fold) rather than visibly here.
///
/// Note the engine has several such ceilings for different paths and they are
/// *not* interchangeable: export folds at 12 (`MAX_NET_CHANNELS`) because an
/// offline render is not bound by the live stack scratch, and the plugin hosts
/// use 16 because a plugin's own bus width is its business.
pub const MAX_SAMPLER_CHANNELS: usize = tutti_core::engine::MAX_ROOT_CHANNELS;

#[macro_use]
mod macros;

mod node_id;

// One mock `Timeline` for every test in the crate, replacing three near-identical
// copies whose constructors disagreed on argument order. Test-only.
#[cfg(test)]
mod test_transport;

// The I/O edge vocabulary is defined once in `tutti-types` and re-exported by
// `tutti-core`; this crate's `WavOut` implements `AudioOut` against it.
pub use tutti_core::io::{pump, AudioIn, AudioOut};

// Voice playback: the two tier units, the mixer over them, and the kernels they
// share. Bevy-free apart from the asset loader, gated inside.
pub mod voice;

// The live audio edge — mic in, WAV out. Independent of voice playback.
pub mod live;

// Time-stretch / pitch-shift (phase vocoder). A peer DSP subsystem, not a
// voice-playback concern: it owns no source and imports nothing from `voice`.
pub mod stretch;

// Bevy-free DSP leaves + value types from `voice` — usable for direct
// FunDSP-graph integration without the ECS layer. Only `WavOut` (the public
// `AudioOut` sink) is re-exported; the butler's `LruCache` / `StreamPin` are
// internal machinery a consumer never constructs, so they stay `pub(crate)`.
pub use live::WavOut;
// `DiskVoiceConfig` and `DiskSource` are not re-exported: nothing
// outside this crate constructs them. `DiskSource` in particular is
// `DiskVoice`'s `inner` — one capability, and only the outer type is
// a doorway.
pub use live::{share_mic_ring, MicMonitorNode, MicRing};
pub use voice::{
    Direction, DiskVoice, LoopSetting, MemorySource, MemorySourceConfig, PendingPlayback, Playback,
    SlotId, TransportPlacement, Voice, VoiceCommand, VoiceNode, VoicePool, VoicePoolHandle,
    VoiceSource,
};
// Bevy ECS surface of `voice`.
#[cfg(feature = "bevy")]
pub use voice::{
    TuttiPlaybackPlugin, VoicePoolNode, VoicePoolRef, WaveAssetLoader, WaveAssetLoaderError,
};

// The async disk-streaming engine (butler thread + the `DiskStreamer` handle). All
// Bevy-free — a non-Bevy host drives it directly via `DiskStreamer::new` / the
// `ButlerThread` API. The `Resource` derive on `DiskStreamer` is `bevy`-gated inside.
pub(crate) mod butler;

mod disk_streamer;
pub use disk_streamer::{DiskStreamer, DiskStreamerConfig};

mod ports;
pub use ports::{Command, Commands, Source, Status};

#[cfg(feature = "bevy")]
use bevy_ecs::prelude::Resource;

/// Transient handed off by `build_into`, part of the umbrella's uniform
/// `PendingX` init handshake: the builder inserts the freshly-built [`DiskStreamer`]
/// and [`TuttiSamplerPlugin`]'s `build()` claims it into a real resource.
#[cfg(feature = "bevy")]
#[derive(Resource, Debug)]
pub struct PendingDiskStreamer(pub Option<DiskStreamer>);

/// Bevy plugin: the sampler's ECS surface — the wave asset loader, time-stretch
/// control sync, and the [`DiskStreamer`] resource handshake.
#[cfg(feature = "bevy")]
#[derive(Debug)]
pub struct TuttiSamplerPlugin;

#[cfg(feature = "bevy")]
impl bevy_app::Plugin for TuttiSamplerPlugin {
    fn build(&self, app: &mut bevy_app::App) {
        app.add_plugins(voice::TuttiPlaybackPlugin);

        // Claim the sampler out of the transient `build_into` inserted
        // (synchronous, during plugin build) — the umbrella `PendingX` handshake.
        if let Some(PendingDiskStreamer(Some(sampler))) =
            app.world_mut().remove_resource::<PendingDiskStreamer>()
        {
            app.insert_resource(sampler);
        }
    }
}

/// The write side's live impl: [`WavOut`], an [`AudioOut`] that streams stereo
/// frames to a WAV file, plus its [`CaptureFormat`](capture::CaptureFormat).
///
/// The record-mic→WAV flow is an explicit [`AudioIn`] → [`AudioOut`] pump
/// driving this sink, lived out by bevy-tutti's `Recorder` (a `MicIn`
/// pumped into a `WavOut` on a background thread).
pub mod capture {
    pub use crate::live::{CaptureFormat, WavOut};
}
