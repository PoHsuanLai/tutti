#![doc = include_str!("../README.md")]
//! The Tutti real-time audio engine behind one dependency.
//!
//! Tutti is a set of focused crates: a graph compiler and executor, a transport,
//! a device layer, DSP nodes, samplers and synths, MIDI, plugin hosting, offline
//! export. This crate re-exports each of them as a module, so a headless consumer
//! — a CLI, an offline renderer, a server, a test — names one dependency and
//! turns subsystems on with features.
//!
//! ```toml
//! [dependencies]
//! tutti = { version = "0.0.1", features = ["device", "decode-wav"] }
//! ```
//!
//! ## Quick start
//!
//! Build a graph, hand its executor to an [`Engine`], and
//! render a block. No device is needed: this is exactly what the audio callback
//! does once per buffer.
//!
//! ```rust
//! use tutti::graph::{GraphBuilder, Prepare};
//! use tutti::nodes::{LfoNode, LfoShape};
//! use tutti::{ChannelLayout, Engine, Hz, InterleavedMut, MaxBlock, SampleRate, Samples, Transport};
//!
//! // A graph with no inputs and a stereo output: a 440 Hz sine on both channels.
//! let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
//! // A node with parameters also hands back its controls (a `ParamSet`), which
//! // set those parameters live from the control thread.
//! let sine = LfoNode::new(LfoShape::Sine).with_rate(Hz(440.0));
//! let (tone, _controls) = g.add_with_controls(sine);
//! g.pipe_output(tone);
//!
//! // Building prepares every node at the render's rate and largest block. The
//! // editor stays on the control thread; the executor goes to the engine.
//! let (mut editor, executor) = g
//!     .build(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples(256))))
//!     .expect("the graph compiles");
//!
//! let transport = Transport::new(SampleRate::SR_48K);
//! let engine = Engine::new(&transport, &mut editor, executor).expect("within the limits");
//!
//! // One interleaved stereo block, rendered on "the audio thread".
//! let mut out = vec![0.0f32; 256 * 2];
//! engine.process(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO));
//! assert!(out.iter().any(|&s| s != 0.0));
//! ```
//!
//! Keep the editor: later edits go through it (`commit`) and reach the engine
//! on its next block.
//!
//! To play through a sound card, turn on `device` and let the session (the
//! `session` feature, on by default) open it and build the graph at the rate it
//! reports: `SessionBuilder::build_device` hands back a `LiveSession`, the
//! `Session` and its `Device` (dropping it stops the stream), and `examples/headless_engine.rs` plays a tone in under twenty
//! lines. To write a file
//! instead, turn on `export` (it brings the `codec` module with it) and bounce the
//! session: a `Bounce` names a range and a master, and optionally stems, and
//! `Session::bounce` renders it (`examples/headless_export.rs`).
//! `examples/song_bounce.rs` is the whole common path, from one
//! `use tutti::{..}`: a sampler `Clip` into a filter into a reverb, an LFO
//! on the cutoff, `Session::play`, and eight beats bounced to a WAV, with `?`
//! through the one `tutti::Error`.
//!
//! ## How the pieces fit
//!
//! - **The graph** ([`graph`]): build one with `GraphBuilder`, or
//!   edit a live one through its `Editor`. Every change is compiled on the
//!   control thread and handed to the `Executor` with `commit`, so the audio
//!   thread never allocates or locks.
//! - **The engine** ([`engine`]): [`Engine`]
//!   renders the executor once per device buffer, folds its output to the
//!   device width, and applies transport commands on their frame.
//!   [`Transport`] ([`transport`])
//!   holds tempo, loop and play state; nodes read the playhead per frame from
//!   their block's `Env`. Level meters and taps are ops in the graph's plan
//!   (`Editor::open_meter`, `Editor::open_tap`); the DSP-load meter is
//!   [`metering`]'s.
//! - **The vocabulary** ([`types`]): unit newtypes (`Hz`, `Db`,
//!   `Beat`, `Samples`, …), channel layouts, interleaved buffer views, and the
//!   real-time primitives the other crates share.
//! - **The nodes** ([`nodes`]) and the instrument crates plug into
//!   the graph as `tutti::graph::Node`s; the edges (`device`, `io`, `export`)
//!   move samples between the graph and the outside world.
//!
//! What an application names is re-exported at the crate's root, so one
//! `use tutti::{Session, SessionEvent, Root, Samples};` covers a program without
//! naming the crate each item lives in; the modules below hold the deeper API.
//!
//! ## The crates, by module
//!
//! | module | crate | what it is | feature |
//! |---|---|---|---|
//! | `tutti::engine` | `tutti-engine` | the per-block graph render: `Engine` | always |
//! | `tutti::transport` | `tutti-transport` | `Transport`, tempo, loop, motion state machine, timed commands, offline timeline | always |
//! | `tutti::metering` | `tutti-metering` | `LoadMeter` | always |
//! | `tutti::types` | `tutti-types` | units, channel layouts, buffer views, RT primitives | always |
//! | `tutti::graph` | `tutti-graph` | the audio graph: builder, compiler, `Editor` / `Executor` | always |
//! | `tutti::nodes` | `tutti-nodes` | built-in nodes: filters, delays, dynamics, LFOs, mixing, automation, VBAP and HRTF panners | always |
//! | `tutti::device` | `tutti-cpal` | the sound card: CPAL output stream, mic capture, driver lifecycle | `device` |
//! | `tutti::codec` | `tutti-codec` | audio file codecs: `Wave`, `FileIn`, the decoders, `FileOut`, the encoders | `codec` (implied by every decode and encode feature and by `export`) |
//! | `tutti::io` | `tutti-session` | recording: `Recorder` and its `PumpLoop` (the sink is `tutti::codec::FileOut`; a graph tap is a `Source` as it is) | `io` |
//! | `tutti::export` | `tutti-export` | the offline render pipeline a `Bounce` drives: one pass of a graph into files or buffers, with progress and cancel | `export` |
//! | `tutti::sampler` | `tutti-sampler` | sample playback: in-memory and streamed voices, time stretch | `sampler` |
//! | `tutti::polysynth` | `tutti-polysynth` | polyphonic subtractive synth node | `synth` |
//! | `tutti::soundfont` | `tutti-soundfont` | SoundFont (.sf2) playback node | `soundfont` |
//! | `tutti::dsp` | `tutti-dsp` | real FFT, windows, filters, dynamics, convolution, delay, reverb | `dsp` |
//! | `tutti::measure` | `tutti-measure` | waveform, onsets, pitch, loudness, correlation, spectra, STFT | `dsp` |
//! | `tutti::modulation` | `tutti-modulation` | modulation sources, routes and the compiler to per-param control programs | `modulation` |
//! | `tutti::ump` | `tutti-ump` | the Universal MIDI Packet and its decoded `Message`, `Midi1Parser` for MIDI 1.0 bytes | `midi` |
//! | `tutti::midi_protocol` | `tutti-midi-protocol` | MIDI 1.0 to 2.0 translation, MPE zones | `midi` |
//! | `tutti::midi_runtime` | `tutti-midi-runtime` | MIDI graph nodes, Beat Clock and MTC sync, JR timestamps, MPE ingest | `midi` |
//! | `tutti::midi_file` | `tutti-midi-file` | Standard MIDI File and MIDI 2.0 Clip File codecs | `midi` |
//! | `tutti::midi_hardware` | `tutti-midi-hardware` | OS MIDI I/O: endpoints, send and receive | `midi-hardware` |
//! | `tutti::midi` | `tutti-session` and the MIDI crates | the common MIDI path's names in one place: `MidiFile`, `Track`, `Message`, `Ump`, `Stamp`, `TapEvent`, `MidiRouteRule`, `MidiEvent`, `midi::Error` | `midi` |
//! | `tutti::plugin` | `tutti-plugin` | VST2, VST3, CLAP and AU plugins as graph nodes | `plugin` + a format |
//!
//! Each module is the whole crate, so its own docs are the reference for it.
//!
//! ## Features
//!
//! None are on by default.
//!
//! - `full` — everything below except `jack` and the plugin features.
//! - `device` — `tutti::device`, the sound card. Off by default so an offline
//!   render, a CI job or a server builds without ALSA or CoreAudio headers.
//! - `jack` — JACK output through `tutti-cpal` (links libjack; implies `device`).
//! - `decode-wav`, `decode-flac`, `decode-mp3`, `decode-ogg` — audio file decoders (each implies `codec`, and
//!   enables the same decoder in the sampler when `sampler` is on).
//! - `encode-wav`, `encode-flac`, `encode-aiff`, `encode-vorbis` — the encoders `tutti::codec` writes
//!   (each implies `codec`; none implies `export`, and `encode-vorbis` is the one path to a C library).
//! - `codec` — `tutti::codec`. Implied by every decode and encode feature.
//! - `io` — `tutti::io`. Implied by `audio-io`.
//! - `audio-io` — mic capture and recording (implies `io`; the capture stream
//!   itself needs `device`).
//! - `export` — `tutti::export` (implies `encode-wav`).
//! - `midi` — `tutti::midi`, `tutti::ump`, `tutti::midi_protocol`, `tutti::midi_runtime`, `tutti::midi_file`. `crates/tutti/examples/midi_all.rs` uses every piece together, offline.
//! - `midi-hardware` — `tutti::midi_hardware` (implies `midi`).
//! - `synth`, `soundfont`, `sampler`, `dsp`, `modulation` — the
//!   matching module (`dsp` gives both `tutti::dsp` and `tutti::measure`). `soundfont` implies `midi`; `sampler` implies `decode-wav`.
//! - `plugin` — `tutti::plugin` (implies `midi`), with one or more formats:
//!   `vst2`, `vst3`, `clap`, `au`. Each format links its SDK, which is why none
//!   is in `full`. The out-of-process plugin server is a separate binary,
//!   `tutti-plugin-server`, built on its own.
//!
//! ## This crate holds no code
//!
//! Every item is a `pub use` of another crate, and the crate's own test enforces
//! it. A convenience constructor or wrapper belongs in the crate that owns the
//! thing it wires — device bootstrap in `tutti-cpal`, graph logic in
//! `tutti-graph` or `tutti-engine` — where it is available to every consumer.
#![deny(missing_docs)]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

// --- the API layer ----------------------------------------------------------

/// The API layer (`tutti-session`): a [`Session`] is a
/// graph, its transport and the engine side it renders on, built offline or
/// on a device by a [`SessionBuilder`]. Feature `session`, on by default.
#[cfg(feature = "session")]
pub use tutti_session as session;

/// The one error a common-path call returns (`tutti_session::Error`): decode,
/// sampler placement, wiring, transport, bounce and the rest each convert into
/// it, so `fn main() -> Result<(), tutti::Error>` takes `?` across a whole
/// program. Feature `session`, on by default.
#[cfg(feature = "session")]
pub use tutti_session::Error;

// --- runtime and vocabulary -------------------------------------------------
//
// Whole-crate re-exports, ALIASED. Flattening would collide at once —
// `Error`/`Result` exist in about eight of these crates, `Unit` in two,
// `Source` in two.

/// The engine (`tutti-engine`): `Engine`, the per-block graph render.
pub use tutti_engine as engine;

/// Playback transport (`tutti-transport`): tempo, loop, the motion state
/// machine, timed commands, the offline timeline.
pub use tutti_transport as transport;

/// The project's tempo (`tutti-tempo`): `TempoMap`, the points and builder it is
/// made from, and the `TempoView` a reader walks it with. What
/// `Session::set_tempo_map` takes.
pub mod tempo {
    pub use tutti_tempo::{TempoMap, TempoMapBuilder, TempoPoint, TempoView, Transition};
}

/// Load metering (`tutti-metering`): `LoadMeter`. Level meters are the
/// graph's (`MeterReader`, `Editor::open_meter`).
pub use tutti_metering as metering;

/// The shared vocabulary (`tutti-types`): unit newtypes, channel layouts,
/// buffer views, the I/O edge traits.
pub use tutti_types as types;

/// Real-time-safe shared-memory primitives for the audio thread (`tutti-rt`):
/// the publish cell (`ArcLend`), the capped `RtVec`, the sequence lock and the
/// denormals flush.
pub use tutti_rt as rt;

/// The audio graph (`tutti-graph`): `GraphBuilder`, `Editor`, `Executor`,
/// and the `Node` trait. What `Engine` runs and what `export` renders.
pub use tutti_graph as graph;

/// Built-in graph nodes (`tutti-nodes`): filters, delays, dynamics, LFOs,
/// mixing, automation, convolution.
pub use tutti_nodes as nodes;

// --- edges ------------------------------------------------------------------

/// The sound card (`tutti-cpal`): CPAL output stream, the audio callback,
/// mic capture and the driver lifecycle. Feature `device`.
#[cfg(feature = "device")]
pub use tutti_cpal as device;

/// File codecs (`tutti-codec`): `Wave`, `FileIn` and the decoders behind them.
/// Feature `codec`, which every format implies.
#[cfg(feature = "codec")]
pub use tutti_codec as codec;

/// The live I/O edge: recording a source into a sink. `Recorder` and the
/// `PumpLoop` it runs (`tutti-session`); the sink is `tutti-codec`'s `FileOut`
/// (`tutti::codec`), and a graph tap is a `Source` as it is. Feature `io`,
/// which `audio-io` implies.
#[cfg(feature = "io")]
pub mod io {
    pub use tutti_session::{
        DitherShape, DitherSink, DitherSpec, IDLE_PARK, PumpLoop, PumpPass, Recorder, RecorderError,
    };
}

/// Offline rendering and export (`tutti-export`). Feature `export`.
#[cfg(feature = "export")]
pub use tutti_export as export;

// --- dsp --------------------------------------------------------------------

/// Sample playback (`tutti-sampler`): in-memory and disk-streamed voices,
/// time stretch. Feature `sampler`.
#[cfg(feature = "sampler")]
pub use tutti_sampler as sampler;

/// The polyphonic subtractive synth node (`tutti-polysynth`). Feature
/// `synth`.
#[cfg(feature = "synth")]
pub use tutti_polysynth as polysynth;

/// A SoundFont 2 file as data (`tutti-sf2`), what `soundfont` plays. Feature
/// `soundfont`.
#[cfg(feature = "soundfont")]
pub use tutti_sf2 as sf2;
/// SoundFont (.sf2) playback (`tutti-soundfont`). Feature `soundfont`.
#[cfg(feature = "soundfont")]
pub use tutti_soundfont as soundfont;

/// Pure DSP over slices (`tutti-dsp`): the real FFT, filters, dynamics,
/// convolution, delay, reverb, windows and the STFT geometry.
/// Feature `dsp`.
#[cfg(feature = "dsp")]
pub use tutti_dsp as dsp;

/// Measurement over slices (`tutti-measure`): waveform blocks, onsets, pitch,
/// loudness, spectra, tempo, key. Feature `dsp`, which turns on both crates.
#[cfg(feature = "dsp")]
pub use tutti_measure as measure;

/// Modulation (`tutti-modulation`): sources, routes and the compiler that turns
/// them into one control program per param. Feature
/// `modulation`.
#[cfg(feature = "modulation")]
pub use tutti_modulation as modulation;

// --- midi -------------------------------------------------------------------

/// The Universal MIDI Packet and its decoded messages (`tutti-ump`): `Ump`,
/// `Message`, `Ump::decode`, `Midi1Parser`. Feature `midi`.
#[cfg(feature = "midi")]
pub use tutti_ump as ump;

/// MIDI 1.0 to MIDI 2.0 translation and the MPE zone types
/// (`tutti-midi-protocol`). Feature `midi`.
#[cfg(feature = "midi")]
pub use tutti_midi_protocol as midi_protocol;

/// The MIDI runtime (`tutti-midi-runtime`): MIDI graph nodes, Beat Clock and MTC
/// sync, JR timestamps, MPE ingest.
/// Feature `midi`.
#[cfg(feature = "midi")]
pub use tutti_midi_runtime as midi_runtime;

/// Standard MIDI File and MIDI 2.0 Clip File codecs (`tutti-midi-file`).
/// Feature `midi`.
#[cfg(feature = "midi")]
pub use tutti_midi_file as midi_file;

/// The names of the common MIDI path in one place:
/// a file (`MidiFile`, `Track`, `Tick`), a message (`Message`, `Ump`,
/// `MidiChannel`, `MidiGroup`, `NoteNumber`, `NoteVelocity`), a timed event, the keys
/// that play an instrument or a live input (`MidiKeys`, sent with a `Stamp`) and a take read with (`TapEvent`, `EventTapReader`),
/// the session's route rule, events and errors (`MidiRouteRule`, `MidiEvent`,
/// `midi::Error`), and under `midi-hardware` a device (`DeviceId`,
/// `DeviceInfo`), what a saved one resolves to (`Resolution`) and what an
/// open one said of itself (`Learned`, `SavedDevice`, `Match`), and the Profiles
/// its MIDI-CI peers support and how to switch one (`PeerProfiles`,
/// `ListedProfile`, `ProfileSwitch`, `ProfilesChange`). The
/// lower layers (`Translator`, `Narrower`, `SysexAssembler`, `ClockMap`) are
/// by path through [`ump`], [`midi_protocol`] and [`midi_runtime`]; the
/// MIDI-CI codec is `tutti-midi-device`'s. Feature `midi`.
#[cfg(feature = "midi")]
pub mod midi {
    pub use tutti_graph::{EventCapacity, EventTapReader, Refused, Stamp, TapEvent};
    pub use tutti_midi_file::{FileEvent, Meta, MidiFile, SmfFormat, Tick, Track};
    pub use tutti_midi_runtime::{MidiKeys, TimedMidiEvent};
    pub use tutti_ump::{Message, MidiChannel, MidiGroup, NoteNumber, NoteVelocity, U7, U14, Ump};

    /// The session's one MIDI error (`tutti_session::MidiError`): each cause
    /// (hardware, file, send, route) is its source.
    #[cfg(feature = "session")]
    pub use tutti_session::MidiError as Error;
    #[cfg(feature = "session")]
    pub use tutti_session::RouteDeclarer;
    #[cfg(feature = "session")]
    pub use tutti_session::{
        AddedInstrument, AnyNode, FollowTempo, InstrumentHandle, Midi, MidiEvent, MidiLoad,
        MidiRouteRule, SessionInstrument, TakePlacement,
    };

    #[cfg(feature = "midi-hardware")]
    pub use tutti_midi_hardware::{
        ChannelCount, ChannelProfile, CiChange, CiPeerInfo, DeviceId, DeviceInfo, DeviceQuery,
        Learned, ListedProfile, Match, PeerProfiles, ProfileError, ProfileSwitch, ProfilesChange,
        SavedDevice, Whole, WholeProfile,
    };
    #[cfg(all(feature = "session", feature = "midi-hardware"))]
    pub use tutti_session::Resolution;
}

/// OS MIDI I/O (`tutti-midi-hardware`): CoreMIDI, ALSA seq-UMP. Feature
/// `midi-hardware`.
#[cfg(feature = "midi-hardware")]
pub use tutti_midi_hardware as midi_hardware;

// --- plugin hosting ---------------------------------------------------------

/// Plugin hosting (`tutti-plugin`): VST2, VST3, CLAP and AU plugins as graph
/// nodes, in-process or sandboxed. Feature `plugin` plus a format feature.
/// The control handle and the in-process node are [`plugin_host`].
///
/// The sandbox's server, `tutti-plugin-server`, is a separate binary the host
/// spawns; build it with `cargo build -p tutti-plugin-server`.
// Not a dependency: that would put a
// `tutti-plugin → tutti-plugin-server → tutti-plugin` cycle one edit away.
#[cfg(feature = "plugin")]
pub use tutti_plugin as plugin;

/// The format-generic half of plugin hosting (`tutti-plugin-host`): the
/// `PluginHandle` control surface, the node's controls, the host policy over a
/// handle (`Liveness`, `PluginParamValues`) and the in-process node. It is the layer
/// the session's views are built on: a host reads and sets a hosted plugin's
/// parameters through `Session::plugin(node)?.params()`, not through the handle. Feature
/// `plugin`.
#[cfg(feature = "plugin")]
pub use tutti_plugin_host as plugin_host;

// --- the application's names, at the root -----------------------------------
//
// What an application names, re-exported here by name, so a program imports
// from one place without knowing which engine crate an item lives in:
// `use tutti::{Session, SessionEvent, Root, Samples};`. There is no prelude
// and no glob: every name is listed, so this file is the surface. Each group
// is behind the feature that gates it. The modules above stay for the deep
// API. `Result`, `Sample` and `Unit` are not here: each would shadow a name a
// program already has. `Phase` is the session's; the oscillator phase unit is
// `tutti::types::Phase`.

// The engine, the transport and the graph's vocabulary.
pub use tutti_engine::Engine;
pub use tutti_graph::{
    CrossfadeCurve, EventCapacity, EventInletNode, EventInletWriter, EventTapId, EventTapReader,
    Level, Losses, MAX_ROOT_CHANNELS, MeterReader, NodeMode, Refused, Stamp, TapAlign, TapEvent,
    TapId, TapReader,
};
pub use tutti_metering::{LoadMeter, LoadReading};
pub use tutti_nodes::{ClickState, MetronomeMode};
pub use tutti_rt::{ArcLend, Loan, Reader};
pub use tutti_tempo::TempoMap;
pub use tutti_transport::{
    FadeOut, LoopEdit, LoopRegion, MotionEvent, MotionState, OfflineTimeline,
    OfflineTimelineConfig, Then, Timeline, Transport, TransportCommand,
};

// Units, channels, buffers, the edge traits and the musical meter.
pub use tutti_types::{
    Amplitude, At, BarNumber, Beat, BeatDuration, Bpm, Cents, CentsSpan, ChannelLayout,
    ChannelTopology, Db, Depth, FiniteSource, Frame, Hz, Interleaved, InterleavedMut, Latency,
    LoopRange, LoopRangeError, MaxBlock, Meter, MeterMap, Moment, Note, NoteValue, Param,
    ParamAddr, PhaseIncrement, PitchClass, Pull, Q, SamplePosition, SampleRate, Samples, Seconds,
    Semitones, Sink, SinkFailed, Source, Speaker, StereoPlanes, Stop, Tail, TimeSignature,
    UnitAddr, UnitParam, Velocity,
};
pub use tutti_ump::{CCNumber, MidiChannel, MidiGroup};

// The device handle and a device's identity.
#[cfg(feature = "device")]
pub use tutti_cpal::{AudioHost, DeviceId, TuttiDriver};

// The sampler.
#[cfg(feature = "sampler")]
pub use tutti_sampler::{Clip, Loop, Playback, Voice, VoiceWindow};

// A decoded wave, and what a bounce writes.
#[cfg(feature = "codec")]
pub use tutti_codec::Wave;
#[cfg(feature = "export")]
pub use tutti_codec::{Format, Pcm};

// The common nodes.
#[cfg(feature = "dsp")]
pub use tutti_nodes::{
    CompressorNode, DelayNode, FdnNode, FreeverbNode, LadderFilterNode, LimiterNode, SvfFilterNode,
    SvfType,
};

// The session, its wiring and its requests.
#[cfg(feature = "session")]
pub use tutti_graph::NodeKey;
#[cfg(feature = "midi")]
pub use tutti_midi_file::{MidiFile, Tick, Track};
#[cfg(feature = "midi")]
pub use tutti_midi_runtime::TimedMidiEvent;
#[cfg(feature = "modulation")]
pub use tutti_modulation::{Follower, Lfo};
#[cfg(feature = "plugin")]
pub use tutti_plugin::catalog::{Catalog, Hosting, PluginRef, PluginState};
#[cfg(feature = "plugin")]
pub use tutti_plugin::{Normalized, ParamId, PluginFailure, PresetError, PresetId};
#[cfg(all(feature = "session", feature = "midi"))]
pub use tutti_session::{AddedInstrument, MidiEvent, MidiRouteRule};
#[cfg(all(feature = "session", feature = "export"))]
pub use tutti_session::{
    Bounce, BounceError, BounceLatency, BounceTail, Bounced, Range, Stem, StemGain,
};
#[cfg(feature = "session")]
pub use tutti_session::{
    ConnectError, Edge, EventInPort, EventOutPort, EventTapPort, HeardOpen, HeardTapError, InPort,
    Job, JobFailed, Key, NoSuchNode, OutPort, Phase, Playhead, Root, ScheduleError, Session,
    SessionBuilder, SessionEvent, TypedParamRef, Workers,
};
#[cfg(all(feature = "session", feature = "device"))]
pub use tutti_session::{Device, DeviceEvent, DeviceRestart, LiveSession, RestartOutcome};
#[cfg(all(feature = "session", feature = "plugin"))]
pub use tutti_session::{
    Diagnostics, EditorView, MainPlugin, MainSession, Opened, ParamGesture, Params, Plugin,
    PluginBuses, PluginChange, PluginEvent, PluginHealth, PluginLoadRequest, PluginNode,
    PluginParamError, PortCounts, PresetEntry, PresetKind, Presets, RecoveryFailure, ReloadCause,
    TypedParam,
};
#[cfg(all(feature = "session", feature = "device", feature = "audio-io"))]
pub use tutti_session::{
    Input, InputConfig, Punch, Recorded, Take, TakeError, TakeEvent, TakeTarget, Trim,
};
#[cfg(all(feature = "session", feature = "modulation"))]
pub use tutti_session::{ParamRef, RouteSpec};
#[cfg(all(feature = "session", feature = "sampler"))]
pub use tutti_session::{SamplerEvent, SamplerLoad};
#[cfg(feature = "midi")]
pub use tutti_ump::{Message, NoteNumber, NoteVelocity, Ump};
