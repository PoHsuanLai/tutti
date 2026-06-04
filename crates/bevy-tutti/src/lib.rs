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
//! `TuttiPlugin` builds the audio engine and inserts each subsystem as its own
//! Bevy resource (`TuttiGraphRes`, `TransportRes`, `MeteringRes`, …). Systems
//! take only the ones they need:
//!
//! ```rust,ignore
//! fn control_audio(transport: Res<TransportRes>, mut graph: ResMut<TuttiGraphRes>) {
//!     transport.tempo(128.0).play();
//!     let id = graph.0.add(tutti_core::dsp::sine_hz(440.0));
//!     graph.0.pipe_output(id);
//!     graph.0.commit();
//! }
//! ```

mod device_state;
mod plugin;

// The graph reconcile hub + `TuttiGraphPlugin` live in `tutti_core::ecs`; the
// leaf reconcilers in their subsystem crates (`tutti_units::ecs`,
// `tutti_sampler::ecs`, `tutti_plugin_host`, `tutti_midi_io::ecs`). `plugin.rs`
// (the composition root) adds them directly — bevy-tutti no longer wraps any of it.
// The export pipeline ECS (StartExport / TuttiExportPlugin) and the offline
// region render (TuttiRegionRenderPlugin) now live in `tutti_export::ecs`;
// bevy-tutti re-exports them via the prelude under the same feature gates.
// The MIDI ECS code (components / events / systems / scheduled dispatch +
// TuttiMidiPlugin) now lives in `tutti_midi_io::ecs`. bevy-tutti re-exports it
// via the prelude under the same feature gates.

/// The Tutti audio engine (CPAL callback, DSP graph, device driver, bootstrap).
/// Relocated here when bevy-tutti became the umbrella crate.
pub mod engine;

pub use plugin::TuttiPlugin;

// Engine types.
pub use engine::{DefaultProcessor, DeviceInfo, Error, Result, TuttiDriver, TuttiGraph};

// bevy-tutti's own UI-mirror resource (audio device state). Its CPAL driver
// is bevy-tutti's, so the mirror lives here. Transport state + master metering
// are dawai projection targets and live in `dawai-model`.
pub use device_state::AudioDeviceState;

// Test-only global allocator for RT-safety regression tests (relocated from
// the umbrella). Panics on any heap allocation inside `assert_no_alloc(..)`
// scopes. One `#[global_allocator]` per binary — this crate's lib-test binary
// owns it.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
