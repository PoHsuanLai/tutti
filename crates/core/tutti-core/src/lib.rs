//! Real-time audio engine core — DSP graph, transport, metering, latency.
//!
//! # Primary API
//!
//! This crate is a vocabulary crate: it owns the DSP graph, transport, metering
//! and latency types, and the sibling crates (tutti-plugin, tutti-sampler, …)
//! build on and re-export them. A consumer wanting the whole engine behind one
//! dependency takes `bevy-tutti`, the umbrella; a consumer wanting audio without
//! Bevy depends on these crates directly.
//!
//! - [`dsp::Net`]: the DSP graph itself (fundsp) — tutti adds no wrapper
//! - [`Transport`]: playback control (play/stop/seek/loop)
//! - [`MasterMeter`] / [`AudioTap`]: audio level monitoring + the analysis tap
//! - [`latency`]: delay compensation — explicit, opt-in, over any graph
//!
//! # Feature-gated APIs
//!
//! All are off by default (`default = []`), which is what keeps this crate
//! Bevy-free unless a consumer asks:
//!
//! - `"bevy"`: a `Component` derive on [`AudioNode`] — see below
//! - `"bevy_asset"`: the above plus fundsp's asset integration
//! - `"wav"` / `"flac"`: fundsp's decoders, for loading a [`Wave`] from disk
//!
//! MIDI is **not** here. The vocabulary lives in `tutti-midi-types`, the state
//! machines in `tutti-midi-runtime`, and the OS edge in `tutti-midi-hardware`.
//!
//! For audio I/O, `tutti-cpal` is the device layer — it opens the stream and
//! wires the real-time callback around this vocabulary.
//!
//! # Example — a graph, a backend, and a transport
//!
//! The engine's centre in one block: build a [`Net`](dsp::Net), wire it, take
//! the audio-thread [`backend`](dsp::Net::backend), then edit the graph and
//! [`commit`](dsp::Net::commit) the edit across to it. `tutti-cpal` does exactly
//! this around a real device; here the backend is pulled by hand, so the whole
//! thing runs headless.
//!
//! ```
//! use tutti_core::dsp::{lowpass_hz, sine_hz, Net};
//! use tutti_core::{AudioUnit, Beat, Bpm, MotionEvent, Timeline, Transport, TransportClock};
//!
//! let sample_rate = 48_000.0;
//! let transport = Transport::new(sample_rate);
//!
//! // The clock is a node: beat-driven sources read musical time off their
//! // input ports rather than consulting the transport, so an offline render
//! // behaves identically to a live one.
//! let mut net = Net::new(0, 2);
//! net.push(Box::new(TransportClock::new(
//!     transport.clock_links(),
//!     sample_rate,
//! )));
//!
//! let source = net.push(Box::new(sine_hz::<f32>(220.0)));
//! let filter = net.push(Box::new(lowpass_hz::<f32>(2_000.0, 0.7)));
//! net.connect(source, 0, filter, 0);
//! // Fans the filter's one output across both device channels; without this
//! // every output edge stays `Port::Zero` and the graph renders silence.
//! net.pipe_output(filter);
//! net.check();
//!
//! // The backend is the audio thread's half. There is exactly one, and after
//! // it exists every frontend edit needs a `commit` to reach it.
//! let mut backend = net.backend();
//! let (left, right) = backend.get_stereo();
//! assert_eq!(left, right);
//!
//! net.connect(source, 0, filter, 0);
//! net.commit();
//!
//! // Transport is a `motion`/`settings` split rather than a `play()` method:
//! // settings anyone may store into, motion a state machine that may defer or
//! // reject. Queued events apply on `drain`, which the audio callback runs.
//! transport.settings.set_tempo(Bpm(90.0));
//! transport.settings.set_beat(Beat(8.0));
//! transport
//!     .motion
//!     .try_send(MotionEvent::Play)
//!     .expect("the motion queue has room at startup");
//! transport.motion.drain();
//!
//! assert!(transport.is_rolling());
//! assert_eq!(transport.beat(), Beat(8.0));
//! ```
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

/// Compatibility path for the unit newtypes (`tutti_core::params::Bpm`, …).
///
/// The vocabulary is [`tutti_types::value`]'s; this module keeps
/// `tutti_core::params::*` imports resolving. Prefer the crate-root re-exports
/// in new code.
pub mod params {
    pub use tutti_types::value::*;
    // `SampleRate` is fundsp's rather than part of tutti-types' value
    // vocabulary, but callers reach for it through this path, so it is carried
    // here too.
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
    TapBusy, TapCons,
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
pub mod codec;

pub mod node_id;

// MIDI vocabulary types (MidiUnitId, MidiIn, MidiOut, …) live in the
// `tutti-midi-types` crate; consumers import them from there directly rather
// than through a tutti-core pass-through.

// The graph-node handle, reachable via the crate root
// (`tutti_core::AudioNode`) or `tutti_core::node::AudioNode`. The DAW param
// components are app-side, not here, which is why this module holds one type.
pub mod node;
pub use node::AudioNode;

// This crate is the engine: fundsp's `Net`, transport, metering, PDC. Wiring it
// into an ECS — the reconcile pipeline, graph resources, per-subsystem
// wrappers — is the host adapter's business, and lives in `bevy_tutti::graph`.
// `AudioNode` above carries a gated `Component` derive because it is the one
// handle a host addresses by name.
