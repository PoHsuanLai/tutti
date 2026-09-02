#![doc = include_str!("../README.md")]

mod error;
pub use error::{Error, Result};

// Parameter vocabulary: the measurement newtypes (Bpm/Hz/Db…), the atomic
// `Param` cell, and the `UnitParam` address enum all live in `tutti-types`
// (pure vocabulary, no engine dependency) and are re-exported here so consumers
// reach them via the engine root. `SampleRate` and the `unit_param` fundsp-glue
// (`setting` / `from_setting`) come from `fundsp-tutti`, which owns fundsp's
// `Setting`.
pub use fundsp::params::SampleRate;
pub use fundsp::unit_param;
pub use tutti_types::value::{
    Amplitude, ArcDegrees, AtomicReadRate, AtomicSamplePosition, Azimuth, Beat, BeatDuration, Bpm,
    Cents, CompressionRatio, Db, Depth, Drive, Elevation, Feedback, Hz, Mix, Pan, Param, ParamAddr,
    Phase, PhaseIncrement, PlaybackRate, Radians, ReadRate, Resonance, SamplePosition, Seconds,
    Semitones, Spread, SrcRatio, StereoWidth, StretchFactor, Unit, UnitParam, Q,
};

mod engine;
// `MAX_ROOT_CHANNELS` comes to the root with `Engine`: it is the ceiling on the
// root's own output width, so a host sizing a scratch buffer for `process` has
// to name it — seven callsites did, all through the module path.
pub use engine::{Engine, MAX_ROOT_CHANNELS};

// The value → runtime seam: `Topology` in, `Net` out. A module rather than root
// re-exports, because `compile` and `Catalog` are words that only read right
// next to the thing they compile — `topology::compile`, not a bare `compile`
// beside `compensate` and `graph_tail`.
pub mod topology;

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

mod metering;
pub use metering::{
    meter_output, AtomicAmplitude, AudioTap, MasterMeter, MeterReading, MeteringContext, TapBusy,
    TapCons,
};

// Delay compensation: the graph-agnostic planner is homed in `tutti-types`,
// the `Net` impls + the delay node in fundsp (where `AudioUnit::latency`
// already lives). Surfaced here so consumers reach both via the engine root.
pub use fundsp::latency::PdcDelay;
pub use tutti_types::latency::{self, Compensation, DelayInsertion, LatencyGraph};
pub use tutti_types::value::Samples;

// How long a graph rings after its input stops. Same split as latency above:
// the walk is graph-agnostic and lives in `tutti-types`, the `Net` impl in
// fundsp beside `AudioUnit::tail`.
pub use tutti_types::tail::{self, graph_tail, GraphTail, TailGraph};
pub use tutti_types::value::Tail;

// The audio graph as a value. Same split again: the value and its folds are
// graph-agnostic and live in `tutti-types`; `topology::compile` below is the
// half that names `Net`.
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

pub mod dsp {
    //! Re-export of fundsp::prelude for DSP building blocks.
    //!
    //! # This is the wall, and it is deliberately the only one
    //!
    //! `fundsp-tutti` is a dependency of `tutti-core` and of nothing else
    //! outside `crates/vendor/**`. Every other crate — engine and adapter alike
    //! — reaches the node contract through this module or through the named
    //! re-exports below it. Adding a `fundsp` dependency to another manifest
    //! puts the fork back into a second crate's public surface and should be
    //! rejected in review; route the need through here instead.
    //!
    //! Note that this is a *glob*. It re-exports fundsp's whole prelude, so the
    //! wall is one of dependency direction, not of surface area: a consumer
    //! cannot name `fundsp`, but it can reach anything the prelude exports.
    //! Narrowing this to an explicit list is worth doing and has not been done.
    //!
    //! # Why the node trait is fundsp's and not tutti's
    //!
    //! The obvious next step — define `AudioUnit` here and let the fork
    //! implement it — does not typecheck, and the reason is worth writing down
    //! so it is not rediscovered:
    //!
    //! - `tutti-core` depends on `fundsp-tutti`, which depends on
    //!   `tutti-types`. Defining the trait in `tutti-core` and having the fork
    //!   consume it is a dependency **cycle**.
    //! - `tutti-types` is the one crate below the fork, and `Tail` and
    //!   [`SampleRate`] already live there for exactly this reason. But
    //!   `AudioUnit<S: Sample>` sits on the fork's `Num`/`Float`/`Real` tower
    //!   (~700 lines, plus `wide`, `libm`, `numeric_array`, `typenum`), and the
    //!   generic is load-bearing — the plugin hosts really do implement
    //!   `AudioUnit<F64>`. `Setting` also carries `Address::Node(NodeId)`, a
    //!   back-edge from the vocabulary into the graph runtime. And
    //!   `tutti-types` is std-only while the fork is `no_std`-capable.
    //! - A separate trait with a blanket impl over the fork's does not work
    //!   either: `Net` stores `Box<dyn AudioUnit>` and *is* an `AudioUnit`, so
    //!   every node would need a wrapper allocation, and `Net::node_as::<T>`
    //!   downcasts (27 sites, including the plugin-host bind path) would see
    //!   the wrapper rather than `T`.
    //!
    //! So lifting the trait is not a re-home; it is gated on two separate
    //! changes to the fork — a `no_std` retrofit of `tutti-types`, and moving
    //! or generifying `Setting`'s `NodeId`.
    //!
    //! # Per-block param delivery (`Env`) — designed, not implemented
    //!
    //! The open question this module inherits: a node currently learns a param
    //! change through [`Setting`], a queued message drained by
    //! `NetBackend::handle_messages` at the top of each `process`. Measurement
    //! (graph plan PR 3) settled two things about it — the 256-slot queue does
    //! *not* overflow under a pumped backend (~750 drains/s against ~60
    //! writes/s), and the "~4 s overflow" is a stalled backend rather than an
    //! automation rate. So the queue is not the problem it was thought to be.
    //!
    //! What it still cannot express is a value that is *read* per block rather
    //! than *pushed* per change: transport frame, sample rate, and the resolved
    //! param set for this block. Delivering those as messages means one queue
    //! entry per param per block, which is the shape that does overflow.
    //!
    //! The design, for when the trait is tutti's own: one `Env { rate, frame,
    //! params }` published whole through [`tutti_types::RtPublish`], with a
    //! trait method taking `&Env` alongside the buffers — the audio thread
    //! takes a single `RtRef` per block and every node reads from it, rather
    //! than each node draining its own mailbox. It cannot be added to the fork's
    //! trait from here: `NetBackend` keeps its `Net` private and exposes no
    //! `set`, so there is no seam through which a host hands one in. That is the
    //! same stop condition PR 3 hit, and it is why this stays a comment.
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
// `Fade` is the graph crossfade path's shape (`Net::crossfade`, and the
// reverb/distortion node rebuilds). It is the only part of fundsp's `sequencer`
// this fork carries — see docs/fundsp-fork-audit.md.
pub use fundsp::sequencer::Fade;
pub use fundsp::setting::Setting;
pub use fundsp::signal::{Signal, SignalFrame};
pub use fundsp::wave::Wave;
pub use fundsp::MAX_BUFFER_SIZE;
pub use fundsp::{Sample, F32, F64};

/// Which audio formats this build can decode.
///
/// Lives here rather than in `tutti-sampler` because this is where the codec
/// features terminate: the sampler's `wav = ["tutti-core/wav"]` forwards, and
/// this crate's `wav = ["fundsp/wav"]` is the line that pulls a decoder in. An
/// answer computed anywhere else is a copy that goes stale.
mod codec;
pub use codec::{can_decode, decodable_extensions};

mod node_id;
// The node-id helpers. `assert_unique` is the one every DSP crate calls from its
// own `node_id` module to prove its ids do not collide; the other three are the
// vocabulary that call sites build ids out of.
pub use node_id::{assert_unique, mnemonic, PDC_DELAY_ID, TRANSPORT_CLOCK_ID};

// MIDI vocabulary types (MidiUnitId, MidiIn, MidiOut, …) live in the
// `tutti-midi-types` crate; consumers import them from there directly rather
// than through a tutti-core pass-through.

// The graph-node handle, reachable as `tutti_core::AudioNode`. The DAW param
// components are app-side, not here, which is why this module holds one type.
mod node;
pub use node::AudioNode;

// This crate is the engine: fundsp's `Net`, transport, metering, PDC. Wiring it
// into an ECS — the reconcile pipeline, graph resources, per-subsystem
// wrappers — is the host adapter's business, and lives in `bevy_tutti::graph`.
// `AudioNode` above carries a gated `Component` derive because it is the one
// handle a host addresses by name.

/// What a host driving the engine names, in one import.
///
/// Not here, and spelled in full instead: `Result`, `Sample` and `Unit` (each
/// would shadow a name a consumer already has), and `dsp` — fundsp's prelude,
/// which stays a module you name, so `Net` is `tutti_core::dsp::Net`.
pub mod prelude {
    pub use tutti_types::prelude::*;

    pub use crate::{AudioNode, AudioUnit, BufferMut, BufferRef, NodeId, SignalFrame};
    pub use crate::{AudioTap, MasterMeter, MeterReading, TapBusy};
    pub use crate::{Engine, Error, MAX_ROOT_CHANNELS};
    pub use crate::{MotionEvent, Timeline, Transport, TransportState};
}
