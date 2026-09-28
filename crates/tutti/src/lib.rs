#![doc = include_str!("../README.md")]
// Every item here is a re-export, which inherits the source item's docs, so
// the only way to trip this is to add something that is not a re-export —
// which `tests/no_logic.rs` forbids anyway. Two gates on the same rule.
//
// This file holds only `pub use`, `pub mod` and docs: a convenience
// constructor belongs in the crate that owns what it wires (device bootstrap
// in `tutti-cpal`, graph logic in `tutti-graph`/`tutti-core`), where
// `bevy-tutti` gets it too. `tests/no_logic.rs` enforces this.
#![deny(missing_docs)]

// --- runtime and vocabulary -------------------------------------------------
//
// Whole-crate re-exports, ALIASED. Flattening would collide at once —
// `Error`/`Result` exist in about eight of these crates, `Unit` in two,
// `Source` in two — and aliasing away the `tutti_` prefix is also what keeps
// `scripts/check-canonical-paths.sh` honest: with no `tutti_x::` spelling
// reachable through this crate, the redundant path it exists to catch cannot
// be written in the first place. `bevy-tutti` aliases the same way.

/// The engine (`tutti-core`): `Engine`, `Transport`, metering, delay
/// compensation.
// Shadows the `core` extern-prelude crate inside this crate only, which is
// harmless because this crate has no code; `::core::` names the real one.
pub use tutti_core as core;

/// The shared vocabulary (`tutti-types`): unit newtypes, channel layouts,
/// buffer views, the I/O edge traits, real-time primitives.
pub use tutti_types as types;

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

/// The I/O edge (`tutti-io`): file decode (`Wave`, `FileIn`), WAV out, mic
/// monitoring and recording. Feature `io`, which `audio-io` and every codec
/// imply.
#[cfg(feature = "io")]
pub use tutti_io as io;

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

/// SoundFont (.sf2) playback (`tutti-soundfont`). Feature `soundfont`.
#[cfg(feature = "soundfont")]
pub use tutti_soundfont as soundfont;

/// Spatial audio (`tutti-spatial`): VBAP speaker panning and binaural HRTF.
/// Feature `spatial`.
#[cfg(feature = "spatial")]
pub use tutti_spatial as spatial;

/// Audio analysis (`tutti-analysis`): waveform, onsets, pitch, loudness.
/// Feature `analysis`.
#[cfg(feature = "analysis")]
pub use tutti_analysis as analysis;

/// Modulation (`tutti-mod`): sources, targets and the mod matrix. Feature
/// `modulation`.
#[cfg(feature = "modulation")]
pub use tutti_mod as modulation;

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
/// device and sampler handles when those features are on. `Result`, `Sample`
/// and `Unit` are left out because each would shadow a name a consumer
/// already has. It matches `bevy-tutti`'s prelude without the ECS types.
pub mod prelude {
    pub use tutti_core::prelude::*;
    pub use tutti_core::transport::{
        ClickState, FadeOut, LoopRange, LoopSpan, MetronomeMode, MotionEvent, MotionState, Then,
    };
    pub use tutti_core::CrossfadeCurve;

    // The device handle and its enumeration record.
    #[cfg(feature = "device")]
    pub use tutti_cpal::{DeviceInfo, TuttiDriver};

    #[cfg(feature = "sampler")]
    pub use tutti_sampler::{Playback, Voice, VoiceWindow};
}
