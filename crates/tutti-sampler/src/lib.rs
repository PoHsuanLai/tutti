//! Sample playback, disk streaming, recording, and time-stretching for the
//! Tutti audio engine.
//!
//! # Quick start
//!
//! ```no_run
//! use tutti_sampler::Sampler;
//!
//! # fn main() -> tutti_sampler::Result<()> {
//! let sampler = Sampler::builder(48_000.0).build()?;
//!
//! // Configure and start playback in one chain — `start()` returns
//! // the channel handle, so post-start tweaks chain naturally.
//! sampler.channel(0)
//!     .play("clip.wav")
//!     .speed(1.5)
//!     .start()
//!     .set_loop(0..96_000)
//!     .seek(44_100);
//!
//! // Record audio to disk.
//! sampler.record("out.wav").channels(2).start().stop();
//! # Ok(())
//! # }
//! ```
//!
//! # Three accessors cover the bulk of the API
//!
//! [`Sampler`] is intentionally narrow. Most operations are reached
//! through one of three handle accessors:
//!
//! | Accessor | Returns | What it owns |
//! |---|---|---|
//! | [`Sampler::channel(n)`](Sampler::channel) | [`play::Channel`] | per-channel playback: play, seek, loop, speed, stop |
//! | [`Sampler::record(path)`](Sampler::record) | [`capture::Builder`] → [`capture::Recording`] | one capture session at a time |
//! | [`Sampler::metrics()`](Sampler::metrics) | [`metrics::View`] | I/O counters, cache stats, per-channel buffer health |
//!
//! Plus subsystem getters when you need finer control:
//! [`recording()`](Sampler::recording),
//! [`automation()`](Sampler::automation),
//! [`audio_input()`](Sampler::audio_input), and
//! [`auditioner()`](Sampler::auditioner) for low-latency previewing.
//!
//! # Crate layout
//!
//! The public API is organized into purpose-named namespaces. The crate
//! root exposes only [`Sampler`] + [`SamplerBuilder`], the DSP graph leaves
//! ([`SamplerUnit`] / [`StreamingSamplerUnit`]), and [`Error`] / [`Result`].
//! Everything else lives in a namespace:
//!
//! - [`play`] — per-channel playback ([`Channel`](play::Channel),
//!   [`Builder`](play::Builder), [`Direction`](play::Direction),
//!   [`Varispeed`](play::Varispeed))
//! - [`capture`] — recording sessions and live recordings
//! - [`metrics`] — I/O counters, cache stats, per-channel health
//! - [`input`] — hardware audio input via `cpal`
//! - [`stretch`] — time-stretch / pitch-shift DSP unit
//! - [`file`](mod@file) — memory-mapped audio-file reader and non-blocking import
//! - [`preview`] — low-latency file-audition player
//!
//! # Direct DSP integration
//!
//! For direct FunDSP-graph integration without the system facade:
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

pub mod asset;
pub use asset::{StreamingProgress, StreamingSample, StreamingSampleProbeError};

pub mod loader;
pub use loader::{
    StreamingSampleLoader, StreamingSampleLoaderError, WaveAssetLoader, WaveAssetLoaderError,
};

/// Bevy ECS integration: sampler-domain components, systems, and plugins.
pub mod ecs;
// Re-export the ECS surface at the crate root so consumers write
// `tutti_sampler::TuttiSamplerPlugin`, not `tutti_sampler::ecs::…` (the
// bevy_text shape — the integration is part of the crate's public API).
pub use ecs::{
    audio_cleanup_system, audio_parameter_sync_system, audio_playback_system,
    bump_param_epoch_sampler, init_auditioner, poll_wave_imports, promote_pending_samplers,
    reconcile_sampler_params, reconcile_sampler_volume, recording_start_system,
    recording_stop_system, AudioInputDeviceInfo, AudioInputState, AudioVolume, AuditionerNode,
    AuditionerRes, ClipCommand, ClipSpec, ContentBounds, DespawnOnFinish, DisableAudioInput,
    EnableAudioInput, PendingSamplerLoad, PlayAudio, PreviewFile, RecordingActive, RecordingResult,
    SamplerRes, SlotId, StartRecording, StopPreview, StopRecording, TimeStretch, TimeStretchControl,
    TrackClipReaderHandle, TrackClipReaderNode, TrackClipReaderRef, TrackClipReaderUnit,
    TuttiAudioInputPlugin, TuttiAuditionerPlugin, TuttiPlaybackPlugin, TuttiRecordingPlugin,
    TuttiSamplerPlugin, TuttiTimeStretchPlugin, WaveImportQueue,
};

pub(crate) mod butler;
mod facade;

#[path = "capture/mod.rs"]
pub(crate) mod capture_impl;

#[path = "input/mod.rs"]
pub(crate) mod input_impl;

mod builder;
mod mmap_reader;
mod units;

#[cfg(feature = "flac")]
pub use builder::flac;
#[cfg(feature = "mp3")]
pub use builder::mp3;
#[cfg(feature = "ogg")]
pub use builder::ogg;
#[cfg(feature = "wav")]
pub use builder::wav;
pub use builder::SampleBuilder;
pub use facade::{Sampler, SamplerBuilder};
pub use units::{PlaybackUnit, SamplerUnit, StreamingSamplerUnit};

/// Per-channel playback: streaming, seeking, looping, varispeed.
pub mod play {
    pub use crate::butler::{PlayDirection as Direction, Varispeed};
    pub use crate::facade::builders::PlayBuilder as Builder;
    pub use crate::facade::channel::Channel;
}

/// Recording sessions and live recording handles.
///
/// Two concepts you may encounter:
/// - [`capture::Recording`] — the *live* recording, returned by
///   [`capture::Builder::start`]. Owns the audio-callback producer; call
///   [`capture::Recording::stop`] or drop it to finalize the file.
/// - [`capture::Session`] — bookkeeping state (punch-in/out, xrun events,
///   preroll) tracked by the [`capture::Recorder`].
pub mod capture {
    pub use crate::capture_impl::config::{
        Config, ConfigBuilder, Mode, QuantizeSettings, QuantizeSettingsBuilder, Source,
    };
    pub use crate::capture_impl::events::Buffer;
    pub use crate::capture_impl::manager::Recorder;
    pub use crate::capture_impl::session::{PunchEvent, Recorded, Session, State, XRun, XRunType};
    pub use crate::facade::builders::{CaptureSession as Recording, RecordBuilder as Builder};

    /// Automation-lane recording (write / touch / latch).
    pub mod automation {
        pub use crate::capture_impl::automation_manager::{AutomationSnapshot, Manager};
        pub use crate::capture_impl::automation_recorder::{AutomationRecordingConfig, Recorder};
        pub use crate::capture_impl::automation_target::{AutomationTarget, RecordingTarget};
    }
}

/// Diagnostics: I/O counters, cache stats, per-channel buffer health.
pub mod metrics {
    pub use crate::butler::{Snapshot as Io, Stats as Cache};
    pub use crate::facade::metrics::View;
}

/// Hardware audio input via `cpal`.
pub mod input {
    pub use crate::input_impl::manager::{Device, Manager};
    pub use crate::input_impl::node::{AudioInput, AudioInputBackend};
}

/// Time-stretching and pitch-shifting DSP unit.
pub mod stretch {
    pub use crate::units::time_stretch::{Algorithm, FftSize, GrainSize, Params, Unit};
}

/// Memory-mapped audio-file reader + non-blocking file import.
///
/// `Format` here describes the in-file *sample encoding* (int16 /
/// int24 / float32) — not the container (WAV / AIFF).
pub mod file {
    pub use crate::facade::import::{ImportHandle, ImportStatus, WaveMetadata};
    pub use crate::mmap_reader::{Format, Info, MmapReader};
}

/// Low-latency file preview player.
pub mod preview {
    pub use crate::facade::auditioner::Auditioner;
}
