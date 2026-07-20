//! Sample playback, disk streaming, recording, and time-stretching for the
//! Tutti audio engine.
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
//!
//! // Record: write a message.
//! recorder.write(StartRecording { channel_index: 0, ..default() });
//!
//! // Preview a browser file: write a message.
//! previews.write(PreviewFile("clip.wav".into()));
//! ```
//!
//! The [`Sampler`] resource is the streaming-engine handle (butler thread +
//! recording / audio-input managers). The engine builds it once with
//! [`Sampler::new`] and inserts it; systems read it as `Res<Sampler>` and reach
//! the subsystems through [`recording()`](Sampler::recording) /
//! [`audio_input()`](Sampler::audio_input), or build an [`Auditioner`] via
//! [`auditioner()`](Sampler::auditioner).
//!
//! # Crate layout
//!
//! Bevy duties are crate-root modules ([`playback`], [`recording`],
//! [`audio_input`], [`auditioner`], [`time_stretch`], [`pending_load`],
//! [`reconcile`], [`track_clip_reader`]). Value types and
//! DSP internals live in purpose-named namespaces:
//!
//! - [`capture`] — recording config + recorder + session bookkeeping
//! - [`input`] — hardware audio input via `cpal`
//! - [`stretch`] — time-stretch / pitch-shift DSP unit
//!
//! # Direct DSP integration
//!
//! For direct FunDSP-graph integration without the ECS layer:
//!
//! ```no_run
//! use std::sync::Arc;
//! use tutti_sampler::{SamplerUnit, stretch};
//! use tutti_core::Wave;
//!
//! let wave = Arc::new(Wave::with_capacity(1, 44_100.0, 0));
//! let unit = SamplerUnit::new(wave);
//! let stretched = stretch::Unit::new(Box::new(unit), 44_100.0);
//! # let _ = stretched;
//! ```

pub mod error;
pub use error::{Error, Result};

mod node_id;

// Each domain is a self-contained module owning its Components / Systems /
// Plugin next to the audio logic it drives (the bevy_audio shape — no `ecs/`
// category folder). `TuttiSamplerPlugin` composes the four domain plugins.
// `playback` stays compiled — it holds the Bevy-free DSP leaves (SamplerUnit,
// StreamingSamplerUnit, TrackClipReaderUnit, stretch::Unit). Its Bevy submodules
// are gated inside `playback/mod.rs`. The other three domains are pure Bevy glue.
pub mod playback;
#[cfg(feature = "bevy")]
pub mod input;
#[cfg(feature = "bevy")]
pub mod preview;
// `recording` (triggers + the capture bookkeeping) is fed by the Bevy-gated
// butler engine, so the whole domain is Bevy-gated.
#[cfg(feature = "bevy")]
pub mod recording;

#[cfg(feature = "bevy")]
pub use input::{
    audio_input_control_system, audio_input_init_system, audio_input_sync_system,
    AudioInputDeviceInfo, AudioInputState, DisableAudioInput, EnableAudioInput,
    TuttiAudioInputPlugin,
};
#[cfg(feature = "bevy")]
pub use preview::streaming_sample::{
    StreamingProgress, StreamingSample, StreamingSampleLoader, StreamingSampleLoaderError,
    StreamingSampleProbeError,
};
#[cfg(feature = "bevy")]
pub use preview::{
    init_auditioner, Auditioner, AuditionerNode, PreviewFile, StopPreview, TuttiAuditionerPlugin,
};
// Bevy-free DSP leaves + value types from `playback` — usable for direct
// FunDSP-graph integration without the ECS layer. (`StreamingSamplerUnit` is
// Bevy-gated: it is fed by the Bevy-gated butler engine.)
pub use playback::{ClipCommand, ClipSpec, SamplerUnit, SlotId,
    TrackClipReaderHandle, TrackClipReaderUnit};
#[cfg(feature = "bevy")]
pub use playback::StreamingSamplerUnit;
// Bevy ECS surface of `playback`.
#[cfg(feature = "bevy")]
pub use playback::{
    audio_cleanup_system, audio_parameter_sync_system, audio_playback_system,
    bump_param_epoch_sampler, poll_wave_imports, promote_pending_samplers, reconcile_sampler_params,
    reconcile_sampler_volume, time_stretch_sync_system, AudioEmitter, AudioPlaybackState,
    AudioVolume, DespawnOnFinish, PendingSamplerLoad, PlayAudio,
    SamplerLooping, SamplerNode, SamplerSpeed,
    TimeStretch, TimeStretchControl, TrackClipReaderNode,
    TrackClipReaderRef, TuttiPlaybackPlugin, WaveImportQueue, WaveAssetLoader,
    WaveAssetLoaderError,
};
#[cfg(feature = "bevy")]
pub use recording::{
    recording_start_system, recording_stop_system, RecordingActive, RecordingResult,
    StartRecording, StopRecording, TuttiRecordingPlugin,
};

// The async disk-streaming engine (butler thread) is reached only through the
// Bevy `Sampler` resource / `preview` domain, and its RT-read primitives are fed
// exclusively by that engine. So the whole streaming subsystem — butler,
// `StreamingSamplerUnit`, the `Sampler` handle — is Bevy-gated. The Bevy-free
// surface is in-memory playback: `SamplerUnit`, `TrackClipReaderUnit`, `stretch`.
// (A framework-free streaming driver would be a separate addition, like the
// `ThreadedWaveCache` path in tutti-wavecache.)
#[cfg(feature = "bevy")]
pub(crate) mod butler;

#[cfg(feature = "bevy")]
mod sampler;
#[cfg(feature = "bevy")]
pub use sampler::{Sampler, SamplerConfig};

#[cfg(feature = "bevy")]
use bevy_ecs::prelude::Resource;

/// Transient handed off by `build_into`. The umbrella builder inserts the
/// freshly-built [`Sampler`]; [`TuttiSamplerPlugin`]'s `build()` derives the
/// [`Auditioner`] from it and installs both as their own resources.
#[cfg(feature = "bevy")]
#[derive(Resource)]
pub struct PendingSampler(pub Option<Sampler>);

/// Bevy plugin: the whole sampler ECS surface.
///
/// Composes the per-duty sub-plugins (playback, recording, audio-input,
/// time-stretch, auditioner) and adds the sampler reconcilers + pending-load
/// promotion + param-epoch bump into the shared `GraphReconcileSystems`
/// schedule owned by [`tutti_core::graph`]. Requires the core graph plugin
/// ([`tutti_core::graph::GraphReconcilePlugin`]) to have configured
/// `GraphReconcileSystems` first.
#[cfg(feature = "bevy")]
pub struct TuttiSamplerPlugin;

#[cfg(feature = "bevy")]
impl bevy_app::Plugin for TuttiSamplerPlugin {
    fn build(&self, app: &mut bevy_app::App) {
        // Each domain is a self-contained plugin; this just composes them.
        // (`SamplerNode` + its params are registered by `TuttiPlaybackPlugin`.)
        // (`TuttiPlaybackPlugin` owns the trigger lifecycle, deferred-load
        // promotion, `SamplerUnit` reconcilers, param-epoch bump, and
        // time-stretch sync.)
        app.add_plugins((
            playback::TuttiPlaybackPlugin,
            recording::TuttiRecordingPlugin,
            input::TuttiAudioInputPlugin,
            preview::TuttiAuditionerPlugin,
        ));

        // Claim the sampler out of the transient `build_into` inserted
        // (synchronous, during plugin build). Derive the auditioner *before*
        // moving the sampler out — `init_auditioner` borrows it.
        if let Some(PendingSampler(Some(sampler))) =
            app.world_mut().remove_resource::<PendingSampler>()
        {
            app.insert_resource(init_auditioner(&sampler));
            app.insert_resource(sampler);
        }
    }
}

/// Recording sessions: capture config + recorder + session bookkeeping.
///
/// Recording is driven the idiomatic Bevy way — write a
/// [`StartRecording`] message — not through a fluent builder. This namespace
/// exposes the value types ([`Config`](capture::Config), [`Source`](capture::Source),
/// [`Mode`](capture::Mode), …) that those messages and the [`Recorder`](capture::Recorder)
/// speak.
#[cfg(feature = "bevy")]
pub mod capture {
    pub use crate::recording::capture::config::{Config, Mode, QuantizeSettings, Source};
    pub use crate::recording::capture::events::Buffer;
    pub use crate::recording::capture::manager::Recorder;
    pub use crate::recording::capture::session::{
        PunchEvent, Recorded, Session, State, XRun, XRunType,
    };
}

/// Time-stretching and pitch-shifting DSP unit.
pub mod stretch {
    pub use crate::playback::time_stretch::{Algorithm, FftSize, GrainSize, Params, Unit};
}
