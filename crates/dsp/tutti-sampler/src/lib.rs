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
//! # Bevy-free use (`--no-default-features`)
//!
//! The whole engine is Bevy-free; only the ECS drivers (Plugins, trigger
//! messages, reconcile systems) are behind the `bevy` feature. A non-Bevy host
//! builds a [`Sampler`] and drives it through plain methods:
//!
//! - In-memory playback: construct a [`SamplerUnit`] / [`TrackClipReaderUnit`]
//!   and add it to a [`tutti_core::graph::AudioGraph`].
//! - **Disk streaming**: [`Sampler::new`] spawns the butler thread;
//!   [`Sampler::auditioner`] gives an [`Auditioner`] whose
//!   [`preview`](Auditioner::preview) streams long files from disk and hands back
//!   a [`StreamingSamplerUnit`] via
//!   [`streaming_unit()`](Auditioner::streaming_unit) to wire into your graph.
//! - Recording: [`Sampler::recording`] returns the capture [`Recorder`](capture::Recorder).
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
pub use error::{Error, RecordingError, Result};

#[macro_use]
mod macros;

mod node_id;

// Each domain is a self-contained module owning its audio engine + (under the
// `bevy` feature) its Components / Systems / Plugin. The disk-streaming engine —
// the butler thread, the `Sampler` handle, `StreamingSamplerUnit`, the recording
// bookkeeper, and the auditioner — is Bevy-free: a non-Bevy host builds a
// `Sampler` with `Sampler::new(..)` and drives streaming/preview through its
// methods directly. Only the ECS drivers (Plugins, trigger Messages, reconcile
// systems) that turn those into entity-spawn workflows are gated.
pub mod playback;
pub mod input;
pub mod preview;
pub mod recording;
pub mod tiering;

// Bevy ECS surface of the input / preview / recording domains.
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
    init_auditioner, AuditionerNode, PreviewFile, StopPreview, TuttiAuditionerPlugin,
};
// The auditioner streaming engine (Bevy-free logic; `Resource` derive is gated).
pub use preview::Auditioner;
// Bevy-free DSP leaves + value types from `playback` — usable for direct
// FunDSP-graph integration without the ECS layer.
pub use butler::{LruCache, StreamPin};
pub use playback::{ClipCommand, ClipReader, ClipSpec, Direction, LoopSetting, SamplerUnit,
    SamplerUnitConfig, SlotId, StreamingClipConfig, StreamingClipReader, StreamingSamplerUnit,
    TransportPlacement, TrackClipReaderHandle, TrackClipReaderUnit};
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

// The async disk-streaming engine (butler thread + the `Sampler` handle). All
// Bevy-free — a non-Bevy host drives it directly via `Sampler::new` / the
// `ButlerThread` API. The `Resource` derive on `Sampler` is `bevy`-gated inside.
pub(crate) mod butler;

mod sampler;
pub use sampler::{Sampler, SamplerConfig};

mod ports;
pub use ports::{ClipControl, Command, Commands, Status};

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
pub mod capture {
    pub use crate::recording::capture::config::{
        CaptureFormat, Config, Mode, QuantizeSettings, Source,
    };
    pub use crate::recording::capture::events::Buffer;
    pub use crate::recording::capture::manager::Recorder;
    pub use crate::recording::capture::session::{
        PunchEvent, Recorded, Session, State, XRun, XRunType,
    };
}

/// Time-stretching and pitch-shifting DSP unit.
pub mod stretch {
    pub use crate::playback::time_stretch::{Algorithm, FftSize, Params, Unit};
}
