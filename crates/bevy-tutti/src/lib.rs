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
//!     let id = graph.0.add(tutti::dsp::sine_hz(440.0));
//!     graph.0.pipe_output(id);
//!     graph.0.commit();
//! }
//! ```

mod loader;
mod metering;
mod transport;
mod device_state;
mod plugin;
mod prelude;
mod resources;

pub mod graph;
pub mod playback;
pub mod dsp;

#[cfg(feature = "analysis")]
mod analysis;
#[cfg(feature = "automation")]
pub mod automation;
#[cfg(feature = "export")]
mod export;
#[cfg(feature = "midi")]
mod midi;
#[cfg(feature = "sampler")]
mod audio_input;
#[cfg(feature = "sampler")]
mod content_bounds;
#[cfg(feature = "sampler")]
mod recording;
#[cfg(feature = "sampler")]
mod time_stretch;
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
#[cfg(all(feature = "plugin", feature = "vst2"))]
pub mod vst2_load;

pub use plugin::TuttiPlugin;
pub use prelude::*;
