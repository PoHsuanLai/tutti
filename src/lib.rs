//! # Tutti — real-time audio engine
//!
//! Tutti is an umbrella crate that binds together modular subsystems behind
//! a single [`TuttiEngine`] bundle.
//!
//! ## Architecture
//!
//! Each subsystem is its own crate, re-exported here as a submodule:
//!
//! | Module                | Subcrate            | Role                                                 |
//! |-----------------------|---------------------|------------------------------------------------------|
//! | [`core`]              | `tutti-core`        | Audio graph runtime, transport, metering, PDC        |
//! | [`dsp`]               | fundsp prelude      | Oscillators, filters, effects, graph operators       |
//! | [`units`]             | `tutti-units`       | Built-in AudioUnits: LFO, filters, dynamics, spatial |
//! | [`midi`]              | `tutti-midi-io`     | Hardware MIDI, virtual ports, SMF, sync decoders     |
//! | [`midi_runtime`]      | `tutti-midi-runtime`| MIDI routing / registry / CC mapping / MPE           |
//! | [`sampler`]           | `tutti-sampler`     | Disk streaming, clip playback, recording             |
//! | [`synth`]             | `tutti-synth`       | Polyphonic synth, SoundFont, wavetable               |
//! | `plugin`              | `tutti-plugin`      | VST2 / VST3 / CLAP hosting (feature `plugin`)        |
//! | [`analysis`]          | `tutti-analysis`    | Pitch / transient / STFT / waveform                  |
//! | [`export`]            | `tutti-export`      | Offline rendering                                    |
//! | [`automation`]        | `tutti-automation`  | Automation curves / modulation                       |
//!
//! The crate root re-exports only the engine-shape items ([`TuttiEngine`],
//! [`TuttiGraph`], [`TuttiDriver`], [`TuttiEngineBuilder`]), the resource
//! builders (`sf2`, `wav`, `vst3`, …), and the narrow set of types
//! that actually appear in those signatures ([`AudioUnit`], [`NodeId`],
//! [`TransportHandle`], [`MeteringHandle`], [`Wave`], [`Fade`], [`Source`]).
//! Everything else is reached through its subsystem module.
//!
//! ## Quick Start
//!
//! [`TuttiEngine`] is a flat bundle of owned subsystems returned from the
//! builder. Destructure it (or address the fields directly) — there is no
//! god-object method surface. Graph edits go through `&mut TuttiGraph` and
//! are staged until [`TuttiGraph::commit`] publishes them to the audio
//! thread. Resources are constructed via free-function builders that take
//! only the subsystem each one needs.
//!
//! ```ignore
//! use tutti::prelude::*;
//!
//! // Sample rate is dictated by the audio device.
//! let mut engine = TuttiEngine::builder().build()?;
//!
//! // Free-function builders — each takes only what it needs.
//! let piano = tutti::sf2(&engine.soundfont, "piano.sf2").preset(0).build()?;
//! let kick  = tutti::wav("kick.wav").gain(0.8).build()?;
//!
//! // Graph edits: &mut TuttiGraph, explicit commit.
//! let piano_id = engine.graph.master(piano);
//! let _kick_id = engine.graph.master(kick);
//!
//! let osc    = engine.graph.add(sine_hz::<f32>(440.0));
//! let filter = engine.graph.add(lowpass_hz::<f32>(2000.0, 1.0));
//! engine.graph.pipe_all(osc, filter);
//! engine.graph.pipe_output(filter);
//! engine.graph.commit();
//!
//! // Handles are cheap to clone / share with ECS resources.
//! engine.transport.play();
//! ```
//!
//! ## Feature flags
//!
//! - `default` — full feature set with `std`
//! - `full` — every feature enabled
//! - `std` — audio I/O via CPAL (auto-enabled by any std-requiring feature)
//! - `midi` — MIDI types and routing
//! - `sampler` — sample playback and recording (requires `std`)
//! - `export` — offline rendering (requires `std`)
//! - `plugin` — plugin hosting (requires `std`)
//! - `analysis` — audio analysis (requires `std`)
//!
//! `tutti = { default-features = false }` compiles `#![no_std]` (requires
//! `alloc`). no_std-clean subcrates: `tutti-core`, `tutti-units`, `tutti-midi`,
//! `fundsp-tutti`, `tutti-automation`.

#![cfg_attr(not(feature = "std"), no_std)]
extern crate alloc;

pub use tutti_core as core;

pub use tutti_units as units;

#[cfg(feature = "midi")]
pub use tutti_midi_io as midi;

#[cfg(feature = "midi")]
pub use tutti_midi_runtime as midi_runtime;

#[cfg(feature = "sampler")]
pub use tutti_sampler as sampler;

#[cfg(feature = "synth")]
pub use tutti_synth as synth;

#[cfg(feature = "plugin")]
pub use tutti_plugin as plugin;

#[cfg(feature = "analysis")]
pub use tutti_analysis as analysis;

#[cfg(feature = "export")]
pub use tutti_export as export;

#[cfg(feature = "automation")]
pub use tutti_units::automation;

/// FunDSP prelude: oscillators (`sine_hz`, `saw_hz`, …), filters
/// (`lowpass_hz`, `moog_hz`, …), effects (`reverb_stereo`, `chorus`, …),
/// noise, envelopes, spatial, dynamics, and the `>>` / `&` / `^` / `|`
/// graph operators.
///
/// See the [fundsp documentation](https://docs.rs/fundsp) for the full list.
pub mod dsp {
    pub use tutti_core::dsp::*;
}

mod error;
pub use error::{Error, Result};

#[cfg(feature = "std")]
mod audio_io;
#[cfg(feature = "std")]
mod builder;
#[cfg(feature = "std")]
mod driver;
#[cfg(feature = "std")]
mod engine;
#[cfg(feature = "std")]
mod graph;
#[cfg(all(feature = "std", feature = "midi", feature = "export"))]
pub mod midi_export;

#[cfg(feature = "std")]
pub use builder::TuttiEngineBuilder;
#[cfg(feature = "std")]
pub use driver::{DeviceInfo, TuttiDriver};
#[cfg(feature = "std")]
pub use engine::{DefaultProcessor, TuttiEngine};
#[cfg(feature = "std")]
pub use graph::TuttiGraph;

// Re-export the per-subsystem resource builders at the crate root so
// `tutti::sf2(...)`, `tutti::wav(...)`, `tutti::vst3(...)`, etc. keep
// working from code that imports the engine bundle. The authoritative
// homes are in the respective subcrates.

#[cfg(all(feature = "std", feature = "soundfont"))]
pub use tutti_synth::{sf2, Sf2Builder};

#[cfg(all(feature = "std", feature = "sampler", feature = "flac"))]
pub use tutti_sampler::flac;
#[cfg(all(feature = "std", feature = "sampler", feature = "mp3"))]
pub use tutti_sampler::mp3;
#[cfg(all(feature = "std", feature = "sampler", feature = "ogg"))]
pub use tutti_sampler::ogg;
#[cfg(all(feature = "std", feature = "sampler", feature = "wav"))]
pub use tutti_sampler::wav;
#[cfg(all(feature = "std", feature = "sampler"))]
pub use tutti_sampler::SampleBuilder;

#[cfg(all(feature = "std", feature = "plugin", feature = "au"))]
pub use tutti_plugin::au;
#[cfg(all(feature = "std", feature = "plugin", feature = "clap"))]
pub use tutti_plugin::clap;
#[cfg(all(feature = "std", feature = "plugin", feature = "vst2"))]
pub use tutti_plugin::vst2;
#[cfg(all(feature = "std", feature = "plugin", feature = "vst3"))]
pub use tutti_plugin::vst3;
#[cfg(all(feature = "std", feature = "plugin"))]
pub use tutti_plugin::PluginBuilder;

pub use tutti_core::{
    AudioUnit, BufferMut, BufferRef, Fade, NodeId, Source, TransportHandle, Wave,
};

#[cfg(feature = "std")]
pub use tutti_core::MeteringHandle;

/// Bevy ECS primitives — components for entity-as-node integration.
///
/// Available only with the `bevy_ecs` feature. The types themselves
/// (`AudioNode`, `NodeKind`, `Volume`, `Pan`, `Mute`, `PluginParam`)
/// live in [`tutti_core::ecs`]; reconcile systems that translate
/// component mutations into graph operations live in `bevy-tutti`.
#[cfg(feature = "bevy_ecs")]
pub use tutti_core::ecs;
#[cfg(feature = "bevy_ecs")]
pub use tutti_core::{
    Attack, AudioNode, Azimuth, CeilingDb, CompressorRatio, DelayTime, Drive, Elevation, Feedback,
    FilterQ, Frequency, GainDb, LayerKey, ModDepth, ModParam, ModRate, Mute, NodeKind, Pan,
    PluginParam, Release, ReverbAlgo, ReverbDamping, ReverbRoomSize, SamplerLooping, SamplerSpeed,
    ThresholdDb, Volume, WetMix,
};

/// Common imports for typical tutti usage.
///
/// Prefer `use tutti::prelude::*;` as the first line of an app that builds
/// and drives a [`TuttiEngine`]. Pulls in the engine types, the fundsp
/// oscillator/filter/effect nodes, and the narrow set of types that appear
/// in engine signatures. Subsystem-specific items are reached through
/// their module (e.g. [`tutti::analysis::PitchDetector`](crate::analysis)).
pub mod prelude {
    #[cfg(feature = "std")]
    pub use crate::{TuttiEngine, TuttiEngineBuilder};

    pub use crate::dsp::*;
    pub use crate::{AudioUnit, BufferMut, BufferRef, Fade, NodeId, Source};
}

// Test-only global allocator for RT-safety regression tests. Panics on
// any heap allocation inside `assert_no_alloc::assert_no_alloc(..)` scopes.
// One `#[global_allocator]` per binary — this crate's lib-test binary
// owns it.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
