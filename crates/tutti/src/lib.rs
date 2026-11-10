#![doc = include_str!("../README.md")]
// Every item here is a re-export, which inherits the source item's docs, so
// the only way to trip this is to add something that is not a re-export —
// which `tests/no_logic.rs` forbids anyway. Two gates on the same rule.
//
// This file holds only `pub use`, `pub mod` and docs: a convenience
// constructor belongs in the crate that owns what it wires, or, when it
// composes several crates, in `tutti-session`, where `bevy-tutti` gets it
// too. `tests/no_logic.rs` enforces this.
#![deny(missing_docs)]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

// --- the API layer ----------------------------------------------------------

/// The API layer (`tutti-session`): a [`Session`](session::Session) is a
/// graph, its transport and the engine side it renders on, built offline or
/// on a device by a [`SessionBuilder`](session::SessionBuilder). The same
/// layer `bevy-tutti` runs on. Feature `session`, on by default.
#[cfg(feature = "session")]
pub use tutti_session as session;

// --- runtime and vocabulary -------------------------------------------------
//
// Whole-crate re-exports, ALIASED. Flattening would collide at once —
// `Error`/`Result` exist in about eight of these crates, `Unit` in two,
// `Source` in two — and aliasing away the `tutti_` prefix is also what keeps
// `scripts/check-canonical-paths.sh` honest: with no `tutti_x::` spelling
// reachable through this crate, the redundant path it exists to catch cannot
// be written in the first place. `bevy-tutti` aliases the same way.

/// The engine (`tutti-engine`): `Engine`, the per-block graph render.
pub use tutti_engine as engine;

/// Playback transport (`tutti-transport`): tempo, loop, the motion state
/// machine, timed commands, the offline timeline.
pub use tutti_transport as transport;

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
/// (`tutti::codec`), and a graph tap is an `AudioIn` as it is. Feature `io`,
/// which `audio-io` implies.
#[cfg(feature = "io")]
pub mod io {
    pub use tutti_session::{IDLE_PARK, PumpLoop, PumpPass, Recorder, RecorderError};
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

/// Spatial audio (`tutti-spatial`): VBAP speaker panning and binaural HRTF.
/// Feature `spatial`.
#[cfg(feature = "spatial")]
pub use tutti_spatial as spatial;

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

/// MIDI value types (`tutti-midi-types`), MIDI 2.0 / UMP native. Feature
/// `midi`.
#[cfg(feature = "midi")]
pub use tutti_midi_types as midi;

/// The MIDI runtime (`tutti-midi-runtime`): MIDI graph nodes, MPE, MIDI-CI.
/// Feature `midi`.
#[cfg(feature = "midi")]
pub use tutti_midi_runtime as midi_runtime;

/// Standard MIDI File and MIDI 2.0 Clip File codecs (`tutti-midi-file`).
/// Feature `midi`.
#[cfg(feature = "midi")]
pub use tutti_midi_file as midi_file;

/// OS MIDI I/O (`tutti-midi-hardware`): CoreMIDI, ALSA seq-UMP. Feature
/// `midi-hardware`.
#[cfg(feature = "midi-hardware")]
pub use tutti_midi_hardware as midi_hardware;

// --- plugin hosting ---------------------------------------------------------

/// Plugin hosting (`tutti-plugin`): VST2, VST3, CLAP and AU plugins as graph
/// nodes, in-process or sandboxed. Feature `plugin` plus a format feature.
///
/// The sandbox's server, `tutti-plugin-server`, is a separate binary the host
/// spawns; build it with `cargo build -p tutti-plugin-server`.
// Not a dependency: that would put a
// `tutti-plugin → tutti-plugin-server → tutti-plugin` cycle one edit away.
#[cfg(feature = "plugin")]
pub use tutti_plugin as plugin;

/// Everything a headless host names, in one import.
///
/// The engine's prelude (units, `ChannelLayout`, buffer views, `Engine`,
/// `Transport`, metering) plus the transport's motion vocabulary, and the
/// device and sampler handles when those features are on, and the session
/// (`Session`, `SessionBuilder`, `SessionEvent`, `Phase`, `GraphSource`, and
/// with `device` its `Device`), with the requests a session takes, each
/// behind the features that gate it: `ParamRef` (`modulation`),
/// `ExportRequest`, `ExportOutput` and `ExportConfig` (`export`),
/// `PluginLoadRequest` and `PluginId` (`plugin`), and `MidiFile` and
/// `TimedMidiEvent` (`midi`), with the offline timeline an export runs on.
/// `Result`, `Sample` and `Unit` are left out because each would shadow a
/// name a consumer already has. It matches `bevy-tutti`'s prelude without
/// the ECS types.
///
/// `Phase` is the session's (`tutti_session::Phase`); the oscillator phase
/// unit is `tutti::types::Phase`.
pub mod prelude {
    pub use tutti_engine::prelude::*;
    pub use tutti_graph::{CrossfadeCurve, Level, MAX_ROOT_CHANNELS, MeterReader, TapReader};
    pub use tutti_metering::{LoadMeter, LoadReading};
    pub use tutti_nodes::{ClickState, MetronomeMode};
    pub use tutti_rt::{ArcLend, Loan, Reader};
    pub use tutti_transport::{
        FadeOut, LoopEdit, LoopRegion, MotionEvent, MotionState, OfflineTimeline,
        OfflineTimelineConfig, Then, Timeline, Transport, TransportCommand,
    };
    pub use tutti_types::prelude::*;
    pub use tutti_types::{At, BarNumber, LoopRange, Meter, TapAlign, TapId};
    pub use tutti_ump::{CCNumber, MidiChannel, MidiGroup};

    // The device handle and its enumeration record.
    #[cfg(feature = "device")]
    pub use tutti_cpal::{DeviceInfo, TuttiDriver};

    #[cfg(feature = "sampler")]
    pub use tutti_sampler::{Playback, Voice, VoiceWindow};

    #[cfg(all(feature = "session", feature = "device"))]
    pub use tutti_session::Device;
    #[cfg(feature = "session")]
    pub use tutti_session::{
        GraphSource, HeardTapError, Phase, Session, SessionBuilder, SessionEvent,
    };

    // What a session's calls take, by the feature that gates each (design
    // 015 §5): a modulation target, an export, a plugin load, a MIDI file.
    #[cfg(feature = "export")]
    pub use tutti_export::ExportConfig;
    #[cfg(feature = "midi")]
    pub use tutti_midi_file::MidiFile;
    #[cfg(feature = "midi")]
    pub use tutti_midi_runtime::TimedMidiEvent;
    #[cfg(feature = "plugin")]
    pub use tutti_plugin::catalog::PluginId;
    #[cfg(all(feature = "session", feature = "modulation"))]
    pub use tutti_session::ParamRef;
    #[cfg(all(feature = "session", feature = "plugin"))]
    pub use tutti_session::PluginLoadRequest;
    #[cfg(all(feature = "session", feature = "export"))]
    pub use tutti_session::{ExportOutput, ExportRequest};
}
