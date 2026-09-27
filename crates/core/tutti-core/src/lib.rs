#![doc = include_str!("../README.md")]

mod error;
pub use error::{Error, Result};

// Parameter vocabulary: the measurement newtypes (Bpm/Hz/Db…), the atomic
// `Param` cell, and the `UnitParam` address enum all live in `tutti-types`
// (pure vocabulary, no engine dependency) and are re-exported here so consumers
// reach them via the engine root. `SampleRate` is one of those units and comes
// from the same place.
pub use tutti_types::value::SampleRate;
pub use tutti_types::value::{
    Amplitude, ArcDegrees, AtomicReadRate, AtomicSamplePosition, Azimuth, Beat, BeatDuration, Bpm,
    Cents, CompressionRatio, Db, Depth, Drive, Elevation, Feedback, Hz, Mix, Pan, Param, ParamAddr,
    Phase, PhaseIncrement, PlaybackRate, Radians, ReadRate, Resonance, SamplePosition, Seconds,
    Semitones, Spread, SrcRatio, StereoWidth, StretchFactor, Unit, UnitParam, Q,
};

mod engine;

// The shape of a node swap: the graph's (`Editor::replace` follows
// it), at the engine root. (`net_fade`, its conversion to fundsp's
// `sequencer::Fade` for `Net::crossfade`, went with the last `Net` fixture
// that crossfaded, doc 013 Phase 3 PR 15; the law is pinned in tutti-graph's
// `fade.rs`.)
pub use tutti_graph::CrossfadeCurve;
// `MAX_ROOT_CHANNELS` comes to the root with `Engine`: it is the ceiling on the
// root's own output width, so a host sizing a scratch buffer for `process` has
// to name it — seven callsites did, all through the module path.
pub use engine::{Engine, GraphEngineError, DEFAULT_BLOCK_CAPACITY, MAX_ROOT_CHANNELS};

pub mod transport;
pub use transport::{
    ClickNode, ClickSettings, ClickState, FadeOut, FrozenClock, LoopRange, MetronomeMode,
    MotionEvent, MotionFsm, MotionState, OfflineTimeline, OfflineTimelineConfig, QueueFull,
    RenderClock, ScheduleFull, Then, Timeline, Transport, TransportClock, TransportCommand,
    TransportSettings, TransportState, SCHEDULE_CAPACITY,
};
// The time a scheduled command names, and the engine's frame clock. Homed in
// `tutti-types` so the graph's `Editor::schedule` and the transport's
// `MotionFsm::schedule` share one vocabulary.
pub use tutti_types::{
    first_frame_at_or_after, snap_to_whole_frame, At, Frame, FrameClock, SegmentOrigin,
    TimelineSegment, FRAME_TOLERANCE,
};

// Musical meter. Lives in `tutti-types` (pure musical math, no audio), re-exported
// here so consumers that already depend on tutti-core need no new dependency.
pub use tutti_types::meter;
pub use tutti_types::meter::{
    BarCount, BarNumber, BarPosition, BeatsPerBar, Meter, MeterChange, MeterMap, NoteValue,
    TimeSignature,
};
pub use tutti_types::{
    PosClaim, PosFrame, PosReader, PosRing, PosWriter, RingWindow, RtPublish, RtRef,
    MAX_POS_RING_FRAMES,
};

mod metering;
pub use metering::{
    meter_output, AtomicAmplitude, AudioTap, MasterMeter, MeterReading, MeteringContext, TapBusy,
    TapCons,
};

// Delay compensation: the graph-agnostic planner, homed in `tutti-types` and
// surfaced here so consumers reach it via the engine root. The delays
// themselves are the graph compiler's (`tutti_graph`'s plan).
pub use tutti_types::latency::{self, Compensation, DelayInsertion, LatencyGraph};
pub use tutti_types::value::Samples;

// How long a graph rings after its input stops. Same as latency above: the
// walk is graph-agnostic and lives in `tutti-types`.
pub use tutti_types::tail::{self, graph_tail, GraphTail, TailGraph};
pub use tutti_types::value::Tail;

// The audio graph as a value, and its folds: graph-agnostic, in `tutti-types`.
// `tutti_graph` compiles it.
pub use tutti_types::graph::{self, NodeKey, Topology, Valid};

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
    AudioThreadCell, RtEventBuf, RtScratch, RtScratchOverflow, RtVec, ScopedNoDenormals,
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

// ── The node contract, from the crate that defines it ───────────────────────
//
// [`AudioUnit`] and everything its signatures name — the planar block buffers,
// the numeric tower they are generic over, the [`Signal`] vocabulary `route`
// speaks — are **`tutti-node`'s**, a leaf crate below the fork, re-exported
// here at the spellings the engine has always used. No graph node implements
// the trait any more (doc 013 Phase 4); what is left of it goes in Phase 5.
pub use tutti_node::buffer::{BufferMut, BufferRef, BufferVec};
pub use tutti_node::signal::{Signal, SignalFrame};
pub use tutti_node::{AudioUnit, FaultLatch, RenderFault, MAX_BUFFER_SIZE};
// The numeric tower the contract is generic over — the part of it consumers
// actually name. `Sample` is the trait's own type parameter and `F32`/`F64` its
// two instantiations (the plugin hosts really do implement `AudioUnit<F64>`);
// `Real` is the bound a filter writes when its coefficient arithmetic is
// generic rather than fixed at f32.
//
// `Num`, `Int` and `Float` are the rest of the tower and are NOT here: nothing
// outside the fork writes those bounds, and every symbol on this list is one
// that had a caller. They are `tutti_node`'s to add back if one appears.
pub use tutti_node::{Real, Sample, F32, F64};

// `Wave`, `FileIn`, `WaveMetadata`, `WaveError`, `WaveAsset` and the
// `can_decode`/`decodable_extensions` pair used to be re-exported here from the
// fork's `wave`/`read`/`stream` modules, with this crate's codec features
// forwarding to `fundsp/…`. They are file I/O, so they moved to `tutti-io`
// with the codec features (design doc 013, Phase 0). This crate decodes nothing.

mod node_id;
// The node-id helpers. `assert_unique` is the one every DSP crate calls from its
// own `node_id` module to prove its ids do not collide; the other two are the
// vocabulary that call sites build ids out of.
pub use node_id::{assert_unique, mnemonic};

// MIDI vocabulary types (MidiEvent, MidiIn, MidiOut, …) live in the
// `tutti-midi-types` crate; consumers import them from there directly rather
// than through a tutti-core pass-through.

// The graph-node handle, reachable as `tutti_core::AudioNode`. The DAW param
// components are app-side, not here, which is why this module holds one type.
mod node;
pub use node::AudioNode;

// This crate is the engine: the render, transport, metering, PDC. Wiring it
// into an ECS — the reconcile pipeline, graph resources, per-subsystem
// wrappers — is the host adapter's business, and lives in `bevy_tutti::graph`.
// `AudioNode` above carries a gated `Component` derive because it is the one
// handle a host addresses by name.

/// What a host driving the engine names, in one import.
///
/// Not here, and spelled in full instead: `Result`, `Sample` and `Unit` (each
/// would shadow a name a consumer already has).
pub mod prelude {
    pub use tutti_types::prelude::*;

    pub use crate::{AudioNode, AudioUnit, BufferMut, BufferRef, SignalFrame};
    pub use crate::{AudioTap, MasterMeter, MeterReading, TapBusy};
    pub use crate::{Engine, Error, MAX_ROOT_CHANNELS};
    pub use crate::{MotionEvent, Timeline, Transport, TransportState};
}
