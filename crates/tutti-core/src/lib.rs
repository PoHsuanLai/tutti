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
//! tutti-core is a std crate that depends on `bevy_ecs`/`bevy_app`: it hosts the
//! shared ECS graph-reconcile hub (`GraphReconcileSystems`, `AudioGraphRes`, the
//! param components, `GraphReconcilePlugin`) that every leaf audio crate schedules
//! against. The DSP/RT vocabulary itself is Bevy-agnostic; the `ecs` module is
//! where the Bevy integration lives.

pub mod error;
pub use error::{Error, Result};

pub mod params;
pub use params::{
    Bpm, Cents, Db, Degrees, Hz, Linear, Ratio, SampleRate, Seconds, Semitones, Unit,
};

mod param;
pub use param::Param;

pub mod processor;
pub use processor::{AudioProcessor, GraphProcessor};

mod graph_net;
pub use graph_net::{CommitOutcome, GraphNet};

pub mod audio_graph;
pub use audio_graph::{isolate_output, GraphDot, AudioGraph};

pub mod transport;
pub use transport::{
    click, AutomationEnvelopeFn, AutomationReaderInput, ClickNode, ClickSettings, ClickState,
    Direction, MetronomeHandle, MetronomeMode, MotionState, OfflineTransport,
    OfflineTransportConfig, SmpteFrameRate, SyncSnapshot, SyncSource, SyncState, SyncStatus,
    TempoMap, TimeSignature, TransportClock, TransportHandle, TransportManager, TransportReader,
    BBT,
};

pub mod metering;
pub use metering::{
    AtomicAmplitude, AtomicStereoAnalysis, CpuMeter, CpuMetrics, MeteringContext, MeteringHandle,
    MeteringManager, StereoAnalysisSnapshot,
};

pub(crate) mod pdc;
pub use pdc::{PdcDelayUnit, PdcManager, PdcState};

pub use atomic_float::{AtomicF32, AtomicF64};
pub use compat::{Arc, AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};

pub use tutti_types::{AudioThreadCell, RtEventBuf};

pub mod rt_scratch;
pub use rt_scratch::{RtScratch, RtScratchOverflow};

pub mod dsp {
    //! Re-export of fundsp::prelude for DSP building blocks.
    pub use fundsp::prelude::*;
}

pub use fundsp::buffer::BufferVec;
pub use fundsp::fft::{inverse_fft, real_fft};
pub use fundsp::math::Complex32;
pub use fundsp::net::{NodeId, Source};
pub use fundsp::prelude::{shared, AudioUnit, BufferMut, BufferRef, Shared};
#[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
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

/// Shared re-exports for the core data structures (parking_lot locks,
/// hashbrown maps, std collections/atomics).
pub mod compat;

pub mod node_id;

pub mod unit_param;
pub use unit_param::UnitParam;

#[cfg(feature = "midi")]
pub mod midi;

#[cfg(feature = "midi")]
pub use midi::MidiUnitId;

#[cfg(feature = "midi")]
pub use midi::{
    MidiInputSource, MidiQueue, MidiRoute, MidiRoutingSnapshot, MidiSource, MidiTarget, NoMidiInput,
};

mod denormals;
pub use denormals::ScopedNoDenormals;

pub mod graph;
pub use graph::{AudioNode, LayerKey, ModParam, Mute, NodeKind, Pan, PluginParam, Volume};

/// Bevy `AsyncComputeTaskPool` + `Task<T>` helper for non-RT subsystem work.
pub mod task;
pub use task::poll_task;
