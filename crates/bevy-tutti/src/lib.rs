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
//!     commands.spawn((PlayAudio { source: assets.load("boom.wav"), ..default() }, DespawnOnFinish));
//!     commands.spawn(PlayAudio { source: assets.load("wind.ogg"), looping: true, gain: 0.3, ..default() });
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
//! Bevy resource (`AudioGraphRes`, `TransportRes`, `MeteringRes`, …). Systems
//! take only the ones they need:
//!
//! ```rust,ignore
//! fn control_audio(transport: Res<TransportRes>, mut graph: ResMut<AudioGraphRes>) {
//!     transport.settings.set_tempo(128.0);
//!     transport.motion.send(MotionEvent::Play);
//!     let id = graph.0.add(tutti_core::dsp::sine_hz(440.0));
//!     graph.0.pipe_output(id);
//!     graph.0.commit();
//! }
//! ```

mod device_state;
mod plugin;

// The graph reconcile hub + `GraphReconcilePlugin` live in `tutti_core::graph`; the
// leaf reconcilers in their subsystem crates (`tutti_units::{dsp, reconcile}`,
// `tutti_sampler::ecs`, `tutti_plugin_host`, and the MIDI subsystem). `plugin.rs`
// (the composition root) adds them directly — bevy-tutti no longer wraps any of it.
// The export pipeline ECS (StartExport / TuttiExportPlugin) and the offline
// region render (TuttiRegionRenderPlugin) now live in `tutti_export::ecs`;
// bevy-tutti re-exports them via the prelude under the same feature gates.
// The MIDI subsystem is grouped by function — input / routing / sequence /
// scheduled / device / mpe sub-plugins composed by `tutti_midi_io::TuttiMidiPlugin`.
// bevy-tutti re-exports it via the prelude under the same feature gates.

/// The Tutti audio engine (CPAL callback, DSP graph, device driver, bootstrap).
/// Relocated here when bevy-tutti became the umbrella crate.
pub mod engine;

pub use plugin::TuttiPlugin;

// Plugin-hosting surface. `tutti-plugin-host` is the Bevy-only plugin-editor /
// scan / crash-detect layer — an implementation detail of this adapter. Consumers
// reach it through bevy-tutti (`bevy_tutti::PluginsRes`, …) rather than depending
// on the engine-workspace crate directly. Re-exported as a namespace + the common
// entry types.
#[cfg(feature = "plugin")]
pub use tutti_plugin_host as plugin_host;
#[cfg(feature = "plugin")]
pub use tutti_plugin_host::{
    OpenPluginEditor, PluginEmitter, PluginsRes, TuttiHostingPlugin,
};

// Engine types.
pub use engine::{DefaultProcessor, DeviceInfo, Error, Result, TuttiDriver, AudioGraph};

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
