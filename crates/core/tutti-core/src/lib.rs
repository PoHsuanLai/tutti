#![doc = include_str!("../README.md")]

mod error;
pub use error::{Error, Result};

// Parameter vocabulary: the measurement newtypes (Bpm/Hz/Db…), the atomic
// `Param` cell, and the `UnitParam` address enum all live in `tutti-types`
// (pure vocabulary, no engine dependency) and are re-exported here so consumers
// reach them via the engine root. `SampleRate` is one of those units and comes
// from the same place; the `unit_param` glue (`setting` / `from_setting`) is
// the fork's, because it converts a `UnitParam` into the `Setting` message
// `Net` delivers.
pub use fundsp::unit_param;
pub use tutti_types::value::SampleRate;
pub use tutti_types::value::{
    Amplitude, ArcDegrees, AtomicReadRate, AtomicSamplePosition, Azimuth, Beat, BeatDuration, Bpm,
    Cents, CompressionRatio, Db, Depth, Drive, Elevation, Feedback, Hz, Mix, Pan, Param, ParamAddr,
    Phase, PhaseIncrement, PlaybackRate, Radians, ReadRate, Resonance, SamplePosition, Seconds,
    Semitones, Spread, SrcRatio, StereoWidth, StretchFactor, Unit, UnitParam, Q,
};

mod engine;

// The shape of a node swap. The curve is the native graph's (its
// `Editor::replace` follows it), re-exported here for the `Net` path;
// `net_fade` converts it to the fork's `sequencer::Fade` for
// `Net::crossfade`, so no crate outside this one names the fork's type.
mod crossfade;
pub use crossfade::net_fade;
pub use tutti_graph::CrossfadeCurve;
// `MAX_ROOT_CHANNELS` comes to the root with `Engine`: it is the ceiling on the
// root's own output width, so a host sizing a scratch buffer for `process` has
// to name it — seven callsites did, all through the module path.
pub use engine::{Engine, GraphEngineError, DEFAULT_GRAPH_BLOCK_CAPACITY, MAX_ROOT_CHANNELS};

// The value → runtime seam: `Topology` in, `Net` out. A module rather than root
// re-exports, because `compile` and `Catalog` are words that only read right
// next to the thing they compile — `topology::compile`, not a bare `compile`
// beside `compensate` and `graph_tail`.
pub mod topology;

pub mod transport;
pub use transport::{
    beat_from_ports, ClickNode, ClickSettings, ClickState, EnvClock, FadeOut, FrozenClock,
    LoopRange, MetronomeMode, MotionEvent, MotionFsm, MotionState, OfflineTimeline,
    OfflineTimelineConfig, QueueFull, RenderClock, ScheduleFull, Then, Timeline, Transport,
    TransportClock, TransportCommand, TransportSettings, TransportState, BEAT_PORTS,
    SCHEDULE_CAPACITY,
};
// The time a scheduled command names, and the engine's frame clock. Homed in
// `tutti-types` so the graph's `Editor::schedule` and the transport's
// `MotionFsm::schedule` share one vocabulary.
pub use tutti_types::{first_frame_at_or_after, At, Frame, TimelineSegment, FRAME_TOLERANCE};

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
    //! The graph runtime and the DSP node library, from `fundsp-tutti`.
    //!
    //! # This is the wall, and it is a named list
    //!
    //! `fundsp-tutti` is a dependency of `tutti-core` and of nothing else
    //! outside `crates/vendor/**`. Every other crate — engine and adapter alike
    //! — reaches the fork through this module. Adding a `fundsp` dependency to
    //! another manifest puts the fork back into a second crate's public surface
    //! and should be rejected in review; route the need through here instead.
    //!
    //! This used to be `pub use fundsp::prelude::*`, which made the wall one of
    //! *dependency direction* and not of surface area: a consumer could not name
    //! `fundsp`, but it could reach anything the prelude exports. It then became
    //! an explicit list of 44 symbols, and design doc 013's Phase 0b took it to
    //! **three**: the graph runtime, `Net`, `NodeId` and `Source`. Nothing else
    //! of the fork is reachable outside this crate, and adding a symbol is a
    //! decision someone makes rather than a side effect of the prelude growing.
    //!
    //! Where each removed name went: the test stimulus (`sine_hz`, `dc`,
    //! `pass`, `split`, `sink`, …) is `tutti_nodes::testing` (behind that
    //! crate's `testing` feature); the filters, limiter, panner, summing bus and
    //! reverb are `tutti-nodes`' own nodes; the distortion curves are
    //! `tutti_nodes::ShapeKind::apply`; the default rate is
    //! `SampleRate::DEFAULT`; `F32x`, `BufferArray` and the `U*` arities gave
    //! way to `BufferVec`; and the operator-DSL combinators the polysynth's
    //! sub-voice was built from went with that sub-voice, when `tutti-polysynth`
    //! moved to its own SoA voice bank. (The crate root lost the fork's `Fade`,
    //! now [`CrossfadeCurve`](crate::CrossfadeCurve), its FFT, now the
    //! sampler's own, its `Shared`/`shared`, and its second copy of
    //! `NodeId`/`Source` in the same pass.)
    //!
    //! **Prefer a Tutti name where one exists.** The node contract is
    //! `tutti-node`'s and is re-exported at the crate root, so a node writes
    //! [`tutti_core::AudioUnit`](crate::AudioUnit), not `dsp::AudioUnit`; a
    //! measurement is `tutti-types`', so a rate is [`SampleRate`](crate::SampleRate) and a frame
    //! count is [`Samples`](crate::Samples). This module is what is left after those.
    //!
    //! # The node contract is no longer here
    //!
    //! [`AudioUnit`](crate::AudioUnit), the planar block buffers, the numeric
    //! tower, [`Signal`](crate::Signal) and [`Setting`](crate::Setting) are
    //! **`tutti-node`'s**, a leaf crate *below* the fork, and `tutti_core`
    //! re-exports them from there. So what this module governs is narrower than
    //! it once was: the graph **runtime** (`Net`, `NetBackend`) and the DSP node
    //! library, not the contract a node implements.
    //!
    //! Three shapes for owning the contract were tried and rejected before that
    //! landed, and the reasons are worth keeping so they are not rediscovered:
    //! defining the trait *here* is a dependency **cycle** (`tutti-core →
    //! fundsp-tutti → tutti-types`); defining it in `tutti-types` drags the
    //! fork's `Num`/`Float`/`Real` tower into the vocabulary crate, and
    //! `AudioUnit<S: Sample>`'s generic is load-bearing (the plugin hosts really
    //! do implement `AudioUnit<F64>`), so it cannot be specialized away; and a
    //! separate trait with a blanket impl breaks `Net`, which stores
    //! `Box<dyn AudioUnit>`, *is* an `AudioUnit`, and downcasts through
    //! `node_as::<T>` — plugin binding and latency, the MIDI endpoint target
    //! and the modulation target and driver in `bevy-tutti`, plus the tests —
    //! and every one of those would see a wrapper rather than `T`.
    //! Putting the contract *below* the fork is the one direction that is none
    //! of those, which is what `tutti-node` does.
    //!
    //! # Per-block param delivery (`Env`) — designed, not implemented
    //!
    //! The open question this module inherits: a node currently learns a param
    //! change through [`Setting`](crate::Setting), a queued message drained by
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
    //! The design: one `Env { rate, frame, params }` published whole through
    //! [`tutti_types::RtPublish`], with a trait method taking `&Env` alongside
    //! the buffers — the audio thread takes a single `RtRef` per block and every
    //! node reads from it, rather than each node draining its own mailbox. The
    //! trait is now `tutti-node`'s, so *adding the method* is finally available.
    //! What still blocks it is the other half, and it is a fork change either
    //! way: `NetBackend` keeps its `Net` private and exposes no `set`, so there
    //! is no seam through which a host hands an `Env` in. That is the same stop
    //! condition graph plan PR 3 hit, and it is why this stays a comment.
    // ── The graph runtime ───────────────────────────────────────────────────
    //
    // What the fork is actually for. `Net` is the runtime graph the value layer
    // compiles into (`tutti_core::topology::compile`) and the adapter drives
    // (`bevy_tutti::graph`); `NodeId` and `Source` are how a wiring declaration
    // names an endpoint. Nothing here has a Tutti equivalent — this *is* the
    // backend.
    //
    // `NetBackend`, the audio-thread half of the RT commit split, is NOT here:
    // it is at the crate root as [`NetBackend`](crate::NetBackend), because a
    // host reaches it to *drive* the engine rather than to build a graph.
    pub use fundsp::net::{Net, NodeId, Source};

    // Deliberately absent, because the engine owns better: the operator-DSL
    // combinators (a second, opaque way to describe a graph beside `Topology`;
    // its last production user, the polysynth sub-voice, is gone), the default
    // rate (`SampleRate::DEFAULT`), the waveshaping curves
    // (`tutti_nodes::ShapeKind::apply`), and fixed-width block scratch
    // (`BufferVec` at the crate root takes its width at runtime). `An<X>`, the
    // `AudioNode` → `AudioUnit` bridge, stays unexported too: write
    // `impl AudioUnit` instead.
}

// ── The node contract, from the crate that defines it ───────────────────────
//
// [`AudioUnit`] and everything its signatures name — the planar block buffers,
// the numeric tower they are generic over, the [`Signal`] vocabulary `route`
// speaks, the [`Setting`] `set` takes — are **`tutti-node`'s**, a leaf crate
// below the fork. They are re-exported here at the spellings the engine has
// always used, so the 42 `impl AudioUnit` sites still say `tutti_core::…` and
// none of them had to change.
//
// This is the half of `tutti_core`'s wall that is no longer a fundsp wall: a
// consumer reaching `tutti_core::AudioUnit` is reaching a Tutti-owned trait.
// What is still fundsp's is the *runtime* below — `Net`, `NetBackend` and the
// DSP node library — and that is what the named `dsp` list above governs.
pub use tutti_node::buffer::{BufferMut, BufferRef, BufferVec};
pub use tutti_node::setting::Setting;
pub use tutti_node::signal::{Signal, SignalFrame};
pub use tutti_node::{AudioUnit, MAX_BUFFER_SIZE};
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

pub use fundsp::realnet::NetBackend;

// `Wave`, `FileIn`, `WaveMetadata`, `WaveError`, `WaveAsset` and the
// `can_decode`/`decodable_extensions` pair used to be re-exported here from the
// fork's `wave`/`read`/`stream` modules, with this crate's codec features
// forwarding to `fundsp/…`. They are file I/O, so they moved to `tutti-io`
// with the codec features (design doc 013, Phase 0). This crate decodes nothing.

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
/// would shadow a name a consumer already has), and `Net` — the fork's graph
/// runtime stays in the module you name, so it is `tutti_core::dsp::Net`.
/// (`dsp::NodeId` is here: a wiring call names endpoints by it.)
pub mod prelude {
    pub use tutti_types::prelude::*;

    pub use crate::dsp::NodeId;
    pub use crate::{AudioNode, AudioUnit, BufferMut, BufferRef, SignalFrame};
    pub use crate::{AudioTap, MasterMeter, MeterReading, TapBusy};
    pub use crate::{Engine, Error, MAX_ROOT_CHANNELS};
    pub use crate::{MotionEvent, Timeline, Transport, TransportState};
}
