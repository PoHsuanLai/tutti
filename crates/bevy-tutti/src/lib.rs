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
//!     // Clip playback runs through tutti-sampler's VoicePool;
//!     // see its docs for building and sending a `Voice`.
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
mod engine_state;
pub mod latency;
mod plugin;

/// Binding the DSP graph to an ECS world: resources, the reconcile pipeline,
/// and the transport / metering wrappers.
pub mod graph;

/// MIDI routing, sequencing, device management and MIDI-CI negotiation.
#[cfg(feature = "midi")]
pub mod midi;

/// Control-rate modulation: LFO sources and mod-matrix edges as ECS entities.
#[cfg(feature = "modulation")]
pub mod modulation;

/// The sampler's asset layer.
#[cfg(feature = "sampler")]
pub mod sampler;

/// SoundFont assets and their playback systems.
#[cfg(feature = "soundfont")]
pub mod synth;

/// Plugin (VST2/VST3/CLAP/AU) editor lifecycle, crash detection and catalog
/// scanning.
#[cfg(feature = "plugin")]
pub mod plugin_host;

/// The Tutti audio engine: CPAL callback, DSP graph, device driver, bootstrap.
pub mod engine;

pub use plugin::TuttiPlugin;

// Latency (plugin delay) compensation. Opt-in: `TuttiPlugin` does not add it,
// because it costs a graph walk per commit and a host with no latency-reporting
// nodes never needs it. See the `latency` module docs for ordering.
pub use latency::{ChannelCompensation, LatencyCompensationPlugin};

#[cfg(feature = "plugin")]
pub use plugin_host::{OpenPluginEditor, PluginEmitter, PluginsRes, TuttiHostingPlugin};

// Engine types. The audio graph itself is `Net` (fundsp) — no wrapper.
pub use engine::{DeviceInfo, Error, Net, Result, TuttiDriver};

/// The audio device's UI-facing mirror. Its CPAL driver is this crate's, so the
/// mirror lives here too.
pub use device_state::AudioDeviceState;

/// Whether the engine is running, and if not, why.
pub use engine_state::AudioEngineState;

/// Everything a typical host needs, in one import.
pub mod prelude {
    pub use crate::graph::{
        commit_graph, crossfade_audio_node, engine_ready, AudioConfig, AudioEmitter, AudioGraphRes,
        AudioPlaybackState, GraphDirty, GraphReconcilePlugin, GraphReconcileSystems, MeteringRes,
        MetronomeRes, NodeParamEpoch, SpawnAudioNode, TransportClockNode, TransportRes,
    };
    pub use crate::{
        AudioDeviceState, AudioEngineState, ChannelCompensation, DeviceInfo,
        LatencyCompensationPlugin, Net, TuttiDriver, TuttiPlugin,
    };

    #[cfg(feature = "midi")]
    pub use crate::midi::{MidiBusRes, MidiRoutingRes, TuttiMidiPlugin};
    #[cfg(feature = "modulation")]
    pub use crate::modulation::{
        ModParamRange, ModRate, ModRoute, ModSource, ModTargetRegistry, ModulationMatrix,
        TuttiModulationPlugin,
    };
    #[cfg(feature = "plugin")]
    pub use crate::plugin_host::{OpenPluginEditor, PluginsRes, TuttiHostingPlugin};

    // The engine vocabulary a host writes graph edits in.
    pub use tutti_core::{AudioNode, NodeId};
}

// Test-only global allocator for RT-safety regression tests (relocated from
// the umbrella). Panics on any heap allocation inside `assert_no_alloc(..)`
// scopes. One `#[global_allocator]` per binary — this crate's lib-test binary
// owns it.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
