//! Sample playback, disk streaming, and time-stretching for the Tutti audio
//! engine.
//!
//! # Two playback tiers, one vocabulary
//!
//! A clip plays either from RAM ([`SamplerUnit`]) or streamed from disk
//! ([`StreamingClipReader`], fed by the butler thread). The tier is the
//! caller's choice — the sampler never picks one on its own — and
//! [`TrackClipReaderUnit`] mixes both behind one command surface; where a verb
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
//! A clip bound to a transport derives its read position from the playhead
//! every frame rather than carrying a cursor, matching `tutti_core`'s transport:
//! one clock advances, everything else reads.
//!
//! The [`Sampler`] handle owns the streaming engine. Build it once with
//! [`Sampler::new`], then drive streaming through the
//! [`commands()`](Sampler::commands) WRITE port and the
//! [`status()`](Sampler::status) READ port.
//!
//! # Crate layout
//!
//! - [`clip`] — the two playback tiers, the per-track mixer, and the shared
//!   interpolation / placement kernels.
//! - [`live`] — mic in, WAV out.
//! - [`stretch`] — phase vocoder (pitch-independent stretch).
//! - [`live`] — mic in, WAV out.
//! - [`AudioIn`] / [`AudioOut`] / [`pump`] — the engine's I/O edge vocabulary,
//!   re-exported from `tutti_types::io`.
//!
//! # Bevy-free use (`--no-default-features`)
//!
//! The engine is Bevy-free; only the ECS drivers are behind the `bevy` feature.
//! A non-Bevy host builds a [`Sampler`] and drives it through plain methods:
//!
//! - In-memory playback: construct a [`SamplerUnit`] / [`TrackClipReaderUnit`]
//!   and add it to a fundsp `Net`.
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

// The I/O edge vocabulary is defined once in `tutti-types` and re-exported by
// `tutti-core`; this crate's `WavOut` implements `AudioOut` against it.
pub use tutti_core::io::{pump, AudioIn, AudioOut};

// Clip playback: the two tier units, the mixer over them, and the kernels they
// share. Bevy-free apart from the asset loader, gated inside.
pub mod clip;

// The live audio edge — mic in, WAV out. Independent of clip playback.
pub mod live;

// Time-stretch / pitch-shift (phase vocoder). A peer DSP subsystem, not a
// clip-playback concern: it owns no source and imports nothing from `clip`.
pub mod stretch;

// Bevy-free DSP leaves + value types from `clip` — usable for direct
// FunDSP-graph integration without the ECS layer. Only `WavOut` (the public
// `AudioOut` sink) is re-exported; the butler's `LruCache` / `StreamPin` are
// internal machinery a consumer never constructs, so they stay `pub(crate)`.
pub use live::WavOut;
// `StreamingClipConfig` and `StreamingSamplerUnit` are not re-exported: nothing
// outside this crate constructs them. `StreamingSamplerUnit` in particular is
// `StreamingClipReader`'s `inner` — one capability, and only the outer type is
// a doorway. `ClipSpec` stays public solely for `tests/rt_no_alloc.rs`, which
// is a separate crate; it has no production caller.
pub use clip::{
    ClipCommand, ClipSpec, Direction, LoopSetting, PendingPlayback, Playback, SamplerUnit,
    SamplerUnitConfig, SlotId, StreamingClipReader, TrackClipReaderHandle, TrackClipReaderUnit,
    TransportPlacement, Voice, VoiceNode, VoiceSource,
};
pub use live::{share_mic_ring, MicMonitorNode, MicRing};
// Bevy ECS surface of `clip`.
#[cfg(feature = "bevy")]
pub use clip::{
    TrackClipReaderNode, TrackClipReaderRef, TuttiPlaybackPlugin, WaveAssetLoader,
    WaveAssetLoaderError,
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
#[derive(Resource, Debug)]
pub struct PendingSampler(pub Option<Sampler>);

/// Bevy plugin: the sampler's ECS surface — the wave asset loader, time-stretch
/// control sync, and the [`Sampler`] resource handshake.
#[cfg(feature = "bevy")]
#[derive(Debug)]
pub struct TuttiSamplerPlugin;

#[cfg(feature = "bevy")]
impl bevy_app::Plugin for TuttiSamplerPlugin {
    fn build(&self, app: &mut bevy_app::App) {
        app.add_plugins(clip::TuttiPlaybackPlugin);

        // Claim the sampler out of the transient `build_into` inserted
        // (synchronous, during plugin build) — the umbrella `PendingX` handshake.
        if let Some(PendingSampler(Some(sampler))) =
            app.world_mut().remove_resource::<PendingSampler>()
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
