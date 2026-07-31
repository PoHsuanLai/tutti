//! Real-time audio engine core — DSP graph, transport, metering, latency.
//!
//! # Primary API
//!
//! This crate is a vocabulary crate. End users typically compose through the
//! `tutti` umbrella crate, which owns a single `TuttiEngine` struct. The
//! vocabulary here (DSP graph, transport, metering, latency, MIDI registry) is
//! re-exported and used by sibling crates (tutti-plugin, tutti-sampler, …).
//!
//! - [`dsp::Net`]: the DSP graph itself (fundsp) — tutti adds no wrapper
//! - [`TransportHandle`]: Playback control (play/stop/seek/loop)
//! - [`MasterMeter`] / [`AudioTap`]: Audio level monitoring + the analysis tap
//! - [`latency`]: Delay compensation — explicit, opt-in, over any graph
//!
//! # Feature-gated APIs
//!
//! - `"midi"`: `MidiBus`, `Midi1Event` for MIDI routing
//!
//! For CPAL audio I/O, use the `tutti` umbrella crate — it owns the device stream
//! and wires the RT callback around this vocabulary.
//!
//! # std + Bevy
//!
//! tutti-core is a std crate whose DSP graph runtime (fundsp's [`Net`](dsp::Net),
//! transport, metering) is Bevy-agnostic. The optional `bevy` feature is **off by
//! default** and adds exactly one thing: a `Component` derive on [`AudioNode`],
//! so an entity can *be* a node in the graph. Everything that reconciles against
//! it — the set hierarchy, the graph resources, the param components, the
//! declarative wiring — lives in the host adapter, `bevy_tutti::graph`. A
//! non-Bevy host wires nodes through `Net`'s `set_source` / `connect` API
//! directly.

pub mod error;
pub use error::{Error, Result};

// Parameter vocabulary: the measurement newtypes (Bpm/Hz/Db…), the atomic
// `Param` cell, and the `UnitParam` address enum all live in `tutti-types`
// (pure vocabulary, no engine dependency) and are re-exported here so consumers
// reach them via the engine root. `SampleRate` and the `unit_param` fundsp-glue
// (`setting` / `from_setting`) come from `fundsp-tutti` (which owns fundsp's
// `Setting`). There is no longer a `tutti_core::param` module — the vocabulary
// has no engine-side home to gather under.
pub use fundsp::params::SampleRate;
pub use fundsp::unit_param;
pub use tutti_types::value::{
    Amplitude, ArcDegrees, AtomicSamplePosition, Azimuth, Beat, BeatDuration, Bpm, Cents,
    CompressionRatio, Db, Depth, Drive, Elevation, Feedback, Hz, Mix, Param, ParamAddr, Phase,
    PhaseIncrement, PlaybackRate, Radians, ReadRate, Resonance, SamplePosition, Seconds, Semitones,
    Spread, SrcRatio, StereoWidth, StretchFactor, Unit, UnitParam, Q,
};

/// Back-compat alias for the unit newtypes' old module path
/// (`tutti_core::params::Bpm`, …). The vocabulary now lives in
/// [`tutti_types::value`]; this keeps existing `tutti_core::params::*` imports
/// resolving. Prefer the crate-root re-exports in new code.
pub mod params {
    pub use tutti_types::value::*;
    // `SampleRate` is fundsp's, not part of tutti-types' value vocabulary, but
    // it belonged to this alias before the move — keep it here.
    pub use fundsp::params::SampleRate;
}

pub mod engine;
pub use engine::Engine;

pub mod transport;
pub use transport::{
    beat_from_ports, ClickNode, ClickSettings, ClickState, FadeOut, FrozenClock, LoopRange,
    MetronomeMode, MotionEvent, MotionFsm, MotionState, OfflineTimeline, OfflineTimelineConfig,
    QueueFull, RenderClock, Then, Timeline, Transport, TransportClock, TransportSettings,
    TransportState, BEAT_PORTS,
};

// Musical meter. Lives in `tutti-types` (pure musical math, no audio), re-exported
// here so consumers that already depend on tutti-core need no new dependency.
pub use tutti_types::meter;
pub use tutti_types::meter::{
    BarCount, BarNumber, BarPosition, BeatsPerBar, Meter, MeterChange, MeterMap, NoteValue,
    TimeSignature,
};
pub use tutti_types::{RtPublish, RtRef};

pub mod metering;
pub use metering::{
    meter_output, AtomicAmplitude, AudioTap, MasterMeter, MeterReading, MeteringContext,
};

// Delay compensation: the graph-agnostic planner is homed in `tutti-types`,
// the `Net` impls + the delay node in fundsp (where `AudioUnit::latency`
// already lives). Surfaced here so consumers reach both via the engine root.
pub use fundsp::latency::PdcDelay;
pub use tutti_types::latency::{self, Compensation, DelayInsertion, LatencyGraph};
pub use tutti_types::value::Samples;

pub use atomic_float::{AtomicF32, AtomicF64};
// Convenience re-exports of the std primitives the RT/DSP vocabulary leans on,
// so sibling crates can write `tutti_core::Arc` etc. (`parking_lot` locks and
// `hashbrown` maps are deliberate non-std choices — sibling crates name those
// crates directly rather than re-exporting them here.)
pub use std::sync::atomic::{
    AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering,
};
pub use std::sync::Arc;

// Real-time audio-thread primitives, all homed in `tutti-types` (the bottom
// leaf, no engine dependency) and surfaced here so consumers reach them via the
// engine root: the one-borrow cell, the event/scratch buffers, the fixed
// scratch, and the denormals guard.
pub use tutti_types::{
    AudioThreadCell, RtEventBuf, RtScratch, RtScratchBuf, RtScratchOverflow, ScopedNoDenormals,
};
// The engine's I/O edge vocabulary (mic/file/plugin sources + sinks), homed in
// `tutti-types` and surfaced here so consumers reach it via the engine root.
pub use tutti_types::io::{self, pump, AudioIn, AudioOut, OnEmpty};
// Float→PCM quantization, homed in `tutti-types` and surfaced here so codecs and
// sinks reach it via the engine root.
pub use tutti_types::pcm::{self, f32_to_i16, f32_to_i24};
// The unified channel-layout enum, homed in `tutti-types` and surfaced here so
// consumers (incl. `bevy-tutti`) reach it via the engine root.
pub use tutti_types::ChannelLayout;
// The general N→device-width surround fold, surfaced so the live host (the CPAL
// callback in `bevy-tutti`) can fold the graph-root buffer to the device width.
// The named-width variants come along: a DSP node that needs one mono sample out
// of a stereo pair (the HRTF/VBAP panners, the convolution node) must reach the
// engine's fold rather than re-deriving `* 0.5` locally, and those crates name
// `tutti-core` as their engine root.
pub use tutti_types::{fold_frame, fold_frame_to_mono, fold_frame_to_stereo};
// The interleaved-buffer views, surfaced for the same reason `ChannelLayout` is:
// `Engine::process` takes an `InterleavedMut`, so every host that drives the
// engine — the CPAL callback above all — names this type at its own boundary.
pub use tutti_types::{Interleaved, InterleavedMut};

pub mod dsp {
    //! Re-export of fundsp::prelude for DSP building blocks.
    pub use fundsp::prelude::*;
}

pub use fundsp::buffer::BufferVec;
pub use fundsp::fft::{inverse_fft, real_fft};
pub use fundsp::math::Complex32;
pub use fundsp::net::{NodeId, Source};
pub use fundsp::prelude::{shared, AudioUnit, BufferMut, BufferRef, Shared};
// `WaveAsset` needs both axes: it is a Bevy `Asset` (so `bevy_asset`), and it
// lives in fundsp's `read` module, which only exists once a codec is on. Gating
// on either alone breaks the other combination.
#[cfg(all(
    feature = "bevy_asset",
    any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg")
))]
pub use fundsp::read::WaveAsset;
// Decode error surfaced by `WaveAsset::from_bytes`; the Bevy `WaveAssetLoader`
// in tutti-sampler wraps it.
#[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
pub use fundsp::read::WaveError;
// `WaveMetadata` is a plain metadata struct (frame count / sample rate /
// channels) — available with any decode feature, no `bevy_asset` needed.
#[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
pub use fundsp::read::WaveMetadata;
// `FileIn` decodes arbitrary sample-frame ranges incrementally from
// disk (real streaming). Butler-thread only. Same codec gating as the rest of
// the decode path.
pub use fundsp::realnet::NetBackend;
#[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
pub use fundsp::stream::FileIn;
// `Fade` is used by the graph crossfade path (`Net::crossfade`,
// reverb/distortion node-rebuild). The rest of `sequencer` (Sequencer/EventId/
// ReplayMode) had no consumers and was dropped — see docs/fundsp-fork-audit.md.
pub use fundsp::sequencer::Fade;
pub use fundsp::setting::Setting;
pub use fundsp::signal::{Signal, SignalFrame};
pub use fundsp::wave::Wave;
pub use fundsp::MAX_BUFFER_SIZE;
pub use fundsp::{Sample, F32, F64};

pub mod node_id;

// MIDI vocabulary types (MidiUnitId, MidiIn, MidiOut, …) live in the
// `tutti-midi-types` crate; consumers import them from there directly rather
// than through a tutti-core pass-through.

// The graph-node handle. Was the `graph` module (params + Bevy hub), but the
// DAW param components moved app-side, leaving only `AudioNode` — so it
// collapsed to this one file. Consumers reach it via the crate root
// (`tutti_core::AudioNode`) or `tutti_core::node::AudioNode`.
pub mod node;
pub use node::AudioNode;

// This crate is the engine: fundsp's `Net`, transport, metering, PDC. Wiring it
// into an ECS — the reconcile pipeline, graph resources, per-subsystem
// wrappers — is the host adapter's business, and lives in `bevy_tutti::graph`.
// `AudioNode` above carries a gated `Component` derive because it is the one
// handle a host addresses by name.
