//! Real-time audio engine core — DSP graph, transport, metering, and PDC.
//!
//! # Primary API
//!
//! This crate is a vocabulary crate. End users typically compose through the
//! `tutti` umbrella crate, which owns a single `TuttiEngine` struct. The
//! vocabulary here (DSP graph, transport, metering, PDC, MIDI registry) is
//! re-exported and used by sibling crates (tutti-plugin, tutti-sampler, …).
//!
//! - [`GraphNet`]: DSP graph manipulation
//! - [`TransportHandle`]: Playback control (play/stop/seek/loop)
//! - [`MeteringManager`]: Audio level monitoring
//! - [`PdcManager`]: Plugin delay compensation
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
//! tutti-core is a std crate whose DSP graph runtime ([`AudioGraph`]/[`GraphNet`],
//! transport, metering, PDC) is Bevy-agnostic. The optional `bevy` feature (on by
//! default) adds the shared ECS graph-reconcile hub (`GraphReconcileSystems`,
//! `AudioGraphRes`, the param components, `GraphReconcilePlugin`) that every leaf
//! audio crate schedules against. Build with `--no-default-features` for a
//! Bevy-free kernel; a non-Bevy host wires nodes via [`AudioGraph`]'s imperative
//! `connect`/`disconnect` API directly.

pub mod error;
pub use error::{Error, Result};

// Parameter vocabulary: units (Bpm/Hz/Db…), the atomic Param cell, and UnitParam
// addressing — grouped under one `param` module by what they do.
pub mod param;
pub use param::{
    Bpm, Cents, Db, Degrees, Hz, Linear, Param, Ratio, SampleRate, Seconds, Semitones, Unit,
    UnitParam,
};

/// Back-compat alias for the unit newtypes' old module path
/// (`tutti_core::params::Bpm`, …). The vocabulary now lives in
/// [`param::units`]; this keeps existing `tutti_core::params::*` imports
/// resolving. Prefer `tutti_core::param::units` (or the crate-root re-exports)
/// in new code.
pub mod params {
    pub use crate::param::units::*;
}

pub mod processor;
pub use processor::{AudioProcessor, GraphProcessor};

pub mod transport;
pub use transport::{
    beat_from_ports, click, AutomationEnvelopeFn, AutomationReaderInput, ClickNode, ClickSettings,
    ClickState, Direction, MetronomeHandle, MetronomeMode, MotionState, OfflineTransport,
    OfflineTransportConfig, TransportClock, TransportClockRead, TransportHandle, TransportManager,
    BEAT_PORTS,
};

pub mod metering;
pub use metering::{
    AtomicAmplitude, AtomicStereoAnalysis, CpuMeter, CpuMetrics, MeteringContext, MeteringHandle,
    MeteringManager, StereoAnalysisSnapshot,
};

pub(crate) mod pdc;
pub use pdc::{PdcDelayUnit, PdcManager, PdcState};

pub use atomic_float::{AtomicF32, AtomicF64};
// Convenience re-exports of the std primitives the RT/DSP vocabulary leans on,
// so sibling crates can write `tutti_core::Arc` etc. (`parking_lot` locks and
// `hashbrown` maps are deliberate non-std choices — sibling crates name those
// crates directly rather than re-exporting them here.)
pub use std::sync::atomic::{
    AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering,
};
pub use std::sync::Arc;

pub use tutti_types::{AudioThreadCell, RtEventBuf, RtScratchBuf};

// Real-time audio-thread primitives: the scratch buffer + the denormals guard.
pub mod rt;
pub use rt::{RtScratch, RtScratchOverflow, ScopedNoDenormals};

pub mod dsp {
    //! Re-export of fundsp::prelude for DSP building blocks.
    pub use fundsp::prelude::*;
}

pub use fundsp::buffer::BufferVec;
pub use fundsp::fft::{inverse_fft, real_fft};
pub use fundsp::math::Complex32;
pub use fundsp::net::{NodeId, Source};
pub use fundsp::prelude::{shared, AudioUnit, BufferMut, BufferRef, Shared};
// `WaveAsset` is a Bevy `Asset` — it only exists in fundsp under `bevy_asset`,
// so gate the re-export on our `bevy_asset` feature (which chains
// `fundsp/bevy_asset`), not on the plain codec features.
#[cfg(feature = "bevy_asset")]
pub use fundsp::read::WaveAsset;
// Decode error surfaced by `WaveAsset::from_bytes`; the Bevy `WaveAssetLoader`
// in tutti-sampler wraps it.
#[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
pub use fundsp::read::WaveError;
// `WaveMetadata` is a plain metadata struct (frame count / sample rate /
// channels) — available with any decode feature, no `bevy_asset` needed.
#[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
pub use fundsp::read::WaveMetadata;
pub use fundsp::realnet::NetBackend;
// `Fade` is used by the graph crossfade path (`AudioGraph::crossfade_boxed`,
// reverb/distortion node-rebuild). The rest of `sequencer` (Sequencer/EventId/
// ReplayMode) had no consumers and was dropped — see docs/fundsp-fork-audit.md.
pub use fundsp::sequencer::Fade;
pub use fundsp::setting::Setting;
pub use fundsp::signal::{Signal, SignalFrame};
pub use fundsp::wave::Wave;
pub use fundsp::MAX_BUFFER_SIZE;
pub use fundsp::{Sample, F32, F64};

pub mod node_id;

// MIDI vocabulary types (MidiTarget, MidiUnitId, MidiQueue, …) live in the
// `tutti-midi-types` crate; consumers import them from there directly rather
// than through a tutti-core pass-through. tutti-core owns only `MidiProcessor`
// (the RT buffer-splitting processor), exported from `processor`.

pub mod graph;
pub use graph::{
    isolate_output, AudioGraph, AudioNode, CommitOutcome, GraphDot, GraphNet, LayerKey, ModParam,
    Mute, NodeKind, Pan, PluginParam, Volume,
};
