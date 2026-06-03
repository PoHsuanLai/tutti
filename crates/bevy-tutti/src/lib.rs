//! Bevy plugin for the Tutti audio engine.
//!
//! Provides ECS components, asset loading, and systems for integrating
//! Tutti into Bevy applications.
//!
//! # Quick Start
//!
//! ```rust,ignore
//! use bevy::prelude::*;
//! use bevy_tutti::*;
//!
//! fn main() {
//!     App::new()
//!         .add_plugins(DefaultPlugins)
//!         .add_plugins(TuttiPlugin::default())
//!         .add_systems(Startup, setup)
//!         .run();
//! }
//!
//! fn setup(mut commands: Commands, assets: Res<AssetServer>) {
//!     commands.spawn(PlayAudio::once(assets.load("boom.wav")).despawn_on_finish());
//!     commands.spawn(PlayAudio::looping(assets.load("wind.ogg")).gain(0.3));
//! }
//! ```
//!
//! # Sub-plugins
//!
//! `TuttiPlugin` is a thin orchestrator: it bootstraps the audio engine,
//! inserts the per-subsystem resources, and adds the sub-plugins for the
//! enabled features. Each duty (playback, MIDI, plugin-host, recording…)
//! is its own `pub Plugin`, so apps that want fine-grained control can opt
//! in à la carte:
//!
//! ```rust,ignore
//! App::new().add_plugins((bevy_tutti::TuttiPlaybackPlugin, bevy_tutti::MidiPlugin));
//! ```
//!
//! # Direct API Access
//!
//! Each subsystem of the `TuttiEngine` is surfaced as its own Bevy resource.
//! Systems take only the ones they need:
//!
//! ```rust,ignore
//! fn control_audio(transport: Res<TransportRes>, mut graph: ResMut<TuttiGraphRes>) {
//!     transport.tempo(128.0).play();
//!     let id = graph.0.add(crate::core::dsp::sine_hz(440.0));
//!     graph.0.pipe_output(id);
//!     graph.0.commit();
//! }
//! ```

mod loader;
mod metering;
mod transport;
pub mod task;
mod device_state;
mod plugin;
mod prelude;
mod resources;

pub mod graph;
// Private: collides with the `bevy_tutti::dsp` subsystem-crate alias below.
// Its public items (TuttiDspPlugin, Add*, dsp_*_system) are surfaced via the
// prelude, and nothing references `bevy_tutti::dsp::*` by path.
mod dsp;

#[cfg(feature = "analysis")]
mod analysis;
// Private: collides with the `bevy_tutti::automation` subsystem-crate alias.
// Public items reach callers via the prelude.
#[cfg(feature = "automation")]
mod automation;
#[cfg(feature = "export")]
mod export;
// Region rendering renders sampler / clip-reader units offline, so it needs the
// sampler subsystem in addition to the export pipeline. Gating it on bare
// `export` made `--features export` fail to compile (it pulls `crate::sampler`
// + `crate::track_clip_reader`, both `sampler`-gated). `full` enables both.
#[cfg(all(feature = "export", feature = "sampler"))]
pub mod render_region;
#[cfg(feature = "midi")]
mod midi;
#[cfg(feature = "soundfont")]
mod soundfont;
#[cfg(feature = "spatial")]
mod spatial;

#[cfg(feature = "plugin")]
pub mod plugin_host;
#[cfg(feature = "plugin")]
pub mod native_window;
#[cfg(all(target_os = "macos", feature = "plugin"))]
mod live_resize;

/// The Tutti audio engine (CPAL callback, DSP graph, device driver, bootstrap).
/// Relocated here when bevy-tutti became the umbrella crate.
pub mod engine;

pub use plugin::TuttiPlugin;
pub use prelude::*;

// =========================================================================
// Engine vocabulary re-export surface.
//
// bevy-tutti is the umbrella: it re-exposes the subsystem crates and the
// engine types under `bevy_tutti::*`, exactly as the old `tutti` umbrella
// did under `crate::*`. Consumers migrate with a `crate::` -> `bevy_tutti::`
// prefix swap. The one non-1:1 case is the fundsp prelude: it is reached as
// `bevy_tutti::core::dsp` (NOT `bevy_tutti::dsp`, which is this crate's own
// DSP systems module).
// =========================================================================

// Subsystem-crate aliases. Only the names that do NOT collide with one of
// bevy-tutti's own modules are aliased here, so a consumer can write
// `bevy_tutti::core` / `::units` / `::sampler` / `::synth` / `::midi_runtime`.
// The colliding ones (midi, plugin, analysis, export, automation) clash with
// bevy-tutti's own modules of the same name — consumers reach those subsystem
// crates by their real crate names directly (`tutti_midi_io`, `tutti_plugin`,
// `tutti_analysis`, `tutti_export`, `tutti_units::automation`), which are all
// first-class workspace members.
pub use tutti_core as core;
pub use tutti_units as units;
#[cfg(feature = "midi")]
pub use tutti_midi_runtime as midi_runtime;
#[cfg(feature = "sampler")]
pub use tutti_sampler as sampler;
#[cfg(feature = "synth")]
pub use tutti_synth as synth;

// Engine types (the umbrella exposed these at its crate root).
pub use engine::{DefaultProcessor, DeviceInfo, Error, Result, TuttiDriver, TuttiEngine, TuttiEngineBuilder, TuttiGraph};

// Narrow set of core types that appear in engine signatures.
pub use tutti_core::{AudioUnit, BufferMut, BufferRef, Fade, MeteringHandle, NodeId, Source, TransportHandle, Wave};

// Entity-as-node ECS primitives + param components (from tutti-core's
// `bevy_ecs` feature). bevy-tutti owns the reconcile systems that translate
// these into graph ops; here we surface them at the root + via `ecs`, matching
// the umbrella.
pub use tutti_core::ecs;
pub use tutti_core::{
    Attack, AudioNode, Azimuth, CeilingDb, CompressorRatio, DelayTime, Drive, Elevation, Feedback,
    FilterQ, Frequency, GainDb, LayerKey, ModDepth, ModParam, ModRate, Mute, NodeKind, Pan,
    PluginParam, Release, ReverbAlgo, ReverbDamping, ReverbRoomSize, SamplerLooping, SamplerSpeed,
    ThresholdDb, Volume, WetMix,
};

// Per-subsystem resource builders, re-exported at the root like the umbrella
// (`bevy_tutti::sf2(..)`, `bevy_tutti::wav(..)`, `bevy_tutti::vst3(..)`, …).
#[cfg(feature = "soundfont")]
pub use tutti_synth::{sf2, Sf2Builder};
#[cfg(all(feature = "sampler", feature = "flac"))]
pub use tutti_sampler::flac;
#[cfg(all(feature = "sampler", feature = "mp3"))]
pub use tutti_sampler::mp3;
#[cfg(all(feature = "sampler", feature = "ogg"))]
pub use tutti_sampler::ogg;
#[cfg(all(feature = "sampler", feature = "wav"))]
pub use tutti_sampler::wav;
#[cfg(feature = "sampler")]
pub use tutti_sampler::SampleBuilder;
#[cfg(all(feature = "plugin", feature = "au"))]
pub use tutti_plugin::au;
#[cfg(all(feature = "plugin", feature = "clap"))]
pub use tutti_plugin::clap;
#[cfg(all(feature = "plugin", feature = "vst2"))]
pub use tutti_plugin::vst2;
#[cfg(all(feature = "plugin", feature = "vst3"))]
pub use tutti_plugin::vst3;
#[cfg(feature = "plugin")]
pub use tutti_plugin::PluginBuilder;

// Test-only global allocator for RT-safety regression tests (relocated from
// the umbrella). Panics on any heap allocation inside `assert_no_alloc(..)`
// scopes. One `#[global_allocator]` per binary — this crate's lib-test binary
// owns it.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
