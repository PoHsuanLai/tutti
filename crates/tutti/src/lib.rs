#![doc = include_str!("../README.md")]
//!
//! # This crate contains no code, and that is the rule
//!
//! Every item here is a `pub use`. There is no `TuttiEngine`, no builder, no
//! driver wrapper, no error aggregation, and there must never be.
//!
//! It was tried the other way once. The package of this name at `9c75ec54`
//! was the **workspace root package** — the root `Cargo.toml` carried both
//! `[workspace]` and `[package] name = "tutti"` — and it held real logic:
//! `TuttiEngine` ("a flat bundle of owned subsystems returned from the
//! builder"), `TuttiGraph`, `TuttiEngineBuilder`, `TuttiDriver`, `audio_io`,
//! `midi_export`, an error type and a `no_std` lattice. It was dissolved in
//! `0a4adf68` ("bevy-tutti … become the umbrella crate") and `4b5bd2fd`
//! ("delete the dissolved tutti umbrella package"), because `bevy-tutti`
//! needed that logic and **two stacked umbrellas, where the lower one owns
//! what the upper one needs, is one umbrella too many.**
//!
//! Re-exporting other crates was never the problem. Owning logic was. So:
//!
//! > If something wants to live here, it belongs in the crate that owns the
//! > thing it wires — device bootstrap in `tutti-cpal`, graph logic in
//! > `tutti-core`.
//!
//! `tests/no_logic.rs` enforces this by reading this file. It is a cheap
//! test for a failure that is one `fn build()` away.
//!
//! # What one dependency does and does not buy
//!
//! One dependency and one `use` line get you the whole vocabulary. They do
//! **not** get you a running audio device in one call, because no such call
//! exists anywhere in the engine: the only bootstrap in the tree is
//! `bevy_tutti::engine::build`, and it inserts Bevy resources. Every *part*
//! is public and Bevy-free already — `AudioEngine::new`,
//! `AudioCallbackState::new`, `TuttiDriver::from_parts` — and assembling
//! them is about thirty lines you can read. `examples/headless_engine.rs` is
//! those thirty lines.
//!
//! That is deliberate rather than unfinished. A `TuttiEngine::builder()` here
//! would be precisely the artifact `4b5bd2fd` deleted.

// Every item here is a re-export, so this costs nothing to satisfy and is the
// cheapest place in the workspace to start enforcing it: a `pub use` inherits
// the source item's docs, so the only way to trip this is to add something
// that is not a re-export — which `tests/no_logic.rs` forbids anyway. Two
// gates on the same rule, from different directions.
#![deny(missing_docs)]

// --- runtime and vocabulary -------------------------------------------------
//
// Whole-crate re-exports, ALIASED. Flattening would collide at once —
// `Error`/`Result` exist in about eight of these crates, `Unit` in two,
// `Source` in two — and aliasing away the `tutti_` prefix is also what keeps
// `scripts/check-canonical-paths.sh` honest: with no `tutti_x::` spelling
// reachable through this crate, the redundant path it exists to catch cannot
// be written in the first place. Same trick `bevy-tutti` uses for
// `tutti_polysynth as polysynth`.

/// The graph runtime: `Net`, `Engine`, `Transport`, metering, latency.
///
/// Shadows the `core` extern-prelude crate *inside this crate only*, which is
/// harmless here because this crate has no code. Write `::core::` in the
/// unlikely event anything needs the real one.
pub use tutti_core as core;

/// The measurement vocabulary: the unit newtypes, channel layouts, the I/O
/// edge traits, `RtPublish`.
pub use tutti_types as types;

/// Planar block buffers and the routing/contract arithmetic.
pub use tutti_node as node;

/// FunDSP's graph and node library, forwarded from `tutti_core::dsp`.
///
/// `Net` stays a name you spell out — `tutti::dsp::Net` — rather than joining
/// the prelude, for the reason `tutti_core`'s own prelude gives for excluding
/// it.
pub use tutti_core::dsp;

/// The DSP node library: LFOs, dynamics, convolution, automation.
pub use tutti_nodes as nodes;

// --- edges ------------------------------------------------------------------

/// The sound card: CPAL streams, the RT callback, driver lifecycle.
#[cfg(feature = "device")]
pub use tutti_cpal as device;

/// The live I/O edge: mic monitor, WAV sink, `Recorder`.
#[cfg(feature = "audio-io")]
pub use tutti_io as io;

/// Offline rendering and export — the live edge's opposite number.
#[cfg(feature = "export")]
pub use tutti_export as export;

// --- dsp --------------------------------------------------------------------

/// Sample playback: streaming, audition, the butler.
#[cfg(feature = "sampler")]
pub use tutti_sampler as sampler;

/// The polyphonic subtractive / wavetable synth.
#[cfg(feature = "synth")]
pub use tutti_polysynth as polysynth;

/// SoundFont (.sf2) playback.
#[cfg(feature = "soundfont")]
pub use tutti_soundfont as soundfont;

/// The engine's geometry: the `vbap` and `hrtf` renderers.
#[cfg(feature = "spatial")]
pub use tutti_spatial as spatial;

/// Audio analysis: waveform, transient, pitch, loudness.
#[cfg(feature = "analysis")]
pub use tutti_analysis as analysis;

/// Pure modulation: the audio-free mod matrix and curves.
#[cfg(feature = "modulation")]
pub use tutti_mod as modulation;

// --- midi -------------------------------------------------------------------

/// MIDI value types, MIDI 2.0 / UMP native.
#[cfg(feature = "midi")]
pub use tutti_midi_types as midi;

/// The MIDI engine: routing, allocation, expression.
#[cfg(feature = "midi")]
pub use tutti_midi_runtime as midi_runtime;

/// SMF and MIDI 2.0 Clip File codecs. OS-free.
#[cfg(feature = "midi")]
pub use tutti_midi_file as midi_file;

/// OS MIDI I/O — CoreMIDI, ALSA seq-UMP.
#[cfg(feature = "midi-hardware")]
pub use tutti_midi_hardware as midi_hardware;

// --- plugin hosting ---------------------------------------------------------

/// Plugin hosting: VST2, VST3, CLAP and AU, in-process or sandboxed.
///
/// `tutti-plugin-server` is deliberately absent: it is a **binary** the host
/// spawns, and depending on it here would put a
/// `tutti-plugin → tutti-plugin-server → tutti-plugin` cycle one edit away.
/// Build it with `cargo build -p tutti-plugin-server`.
#[cfg(feature = "plugin")]
pub use tutti_plugin as plugin;

/// Everything a headless host names, in one import.
///
/// The exclusions are not oversights — they are `tutti_core`'s and
/// `tutti_types`' own, forwarded unchanged. `Result`, `Sample` and `Unit`
/// each shadow a name a consumer already has, and `Net` stays spelled out
/// (through this crate, `tutti::dsp::Net`).
///
/// This is exactly `bevy_tutti`'s prelude with the ECS group removed, which
/// is the point: it is not a new design, it is the Bevy-free half of a
/// prelude already in production use.
pub mod prelude {
    pub use tutti_core::prelude::*;
    pub use tutti_core::transport::{
        beat_from_ports, ClickState, FadeOut, LoopRange, LoopSpan, MetronomeMode, MotionEvent,
        MotionState, Then, BEAT_PORTS,
    };
    pub use tutti_core::CrossfadeCurve;

    /// The device handle and its enumeration record. `bevy-tutti` surfaces
    /// both at *its* root; a headless host needs them at least as much.
    #[cfg(feature = "device")]
    pub use tutti_cpal::{DeviceInfo, TuttiDriver};

    #[cfg(feature = "sampler")]
    pub use tutti_sampler::{Playback, Voice, VoiceWindow};
}
