//! Bevy plugin for the Tutti audio engine.
//!
//! Provides ECS components, asset loading, and systems for integrating
//! Tutti into Bevy applications.
//!
//! # Quick Start
//!
//! Add [`TuttiPlugin`], spawn a node, and declare what reaches the speakers.
//! `disabled: true` opens no device, which is what makes this run in CI — a real
//! host drops that field and everything else stays the same.
//!
//! ```rust
//! use bevy_app::prelude::*;
//! use bevy_ecs::prelude::*;
//! use bevy_tutti::prelude::*;
//! use tutti_core::Hz;
//! use tutti_nodes::testing::Osc;
//!
//! fn build_chain(mut commands: Commands) {
//!     let osc = commands.spawn_audio_node(Osc::sine(Hz(440.0))).id();
//!     // Wiring is *declared*, never called: the resource names what feeds each
//!     // global output channel, so two nodes cannot both claim the master.
//!     commands.insert_resource(MasterSources::mono_from(osc));
//! }
//!
//! let mut app = App::new();
//! // Ordinary Bevy prerequisites, not tutti's: a subsystem that registers an
//! // asset loader needs an `AssetServer`, and one that runs IO off the main
//! // thread needs the task pools. A real host has both from `DefaultPlugins`.
//! app.add_plugins((bevy_app::TaskPoolPlugin::default(), bevy_asset::AssetPlugin::default()));
//! app.add_plugins(TuttiPlugin { disabled: true, ..Default::default() });
//! // The device is off, so stand the graph up by hand — these are the two
//! // resources `build_into` would have inserted. The reconcile schedule is
//! // already there: `TuttiPlugin` adds it either way.
//! app.insert_resource(AudioGraphRes::headless(0, 2));
//! app.insert_resource(AudioEngineState::Running);
//! app.add_systems(Startup, build_chain);
//! app.update();
//!
//! let node = *app.world_mut().query::<&AudioNode>().single(app.world()).unwrap();
//! let graph = app.world().resource::<AudioGraphRes>();
//! // Read the edge back off the engine, not off the component.
//! assert_eq!(graph.output_source(0), GraphSource::Node(node, 0));
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
//! ```rust
//! # use bevy_app::prelude::*;
//! // `GraphReconcilePlugin` is the always-present one; the feature-gated
//! // subsystems (`TuttiPlaybackPlugin`, `TuttiMidiPlugin`, …) join it the same
//! // way when their feature is built.
//! App::new().add_plugins(bevy_tutti::graph::GraphReconcilePlugin);
//! ```
//!
//! # Direct API Access
//!
//! `TuttiPlugin` builds the audio engine and inserts each subsystem as its own
//! Bevy resource (`AudioGraphRes`, `TransportRes`, `MeteringRes`, …). Systems
//! take only the ones they need:
//!
//! ```rust
//! # use bevy_ecs::prelude::*;
//! # use bevy_tutti::prelude::*;
//! # use tutti_core::transport::MotionEvent;
//! fn control_audio(
//!     transport: Res<TransportRes>,
//!     mut graph: ResMut<AudioGraphRes>,
//!     mut dirty: ResMut<GraphDirty>,
//! ) {
//!     transport.settings.set_tempo(128.0);
//!     // `try_send` — the motion queue is bounded, so a send can fail and the
//!     // caller decides what that means.
//!     let _ = transport.motion.try_send(MotionEvent::Play);
//!     let node = graph.insert(tutti_nodes::testing::Osc::sine(tutti_core::Hz(440.0)));
//!     // Staged, not committed: `commit_graph` publishes the frame's edits once.
//!     dirty.0 = true;
//!     let _ = node;
//! }
//! ```
//!
//! A node added this way is **unwired** and renders nothing. What feeds it, and
//! what reaches the speakers, is declared — see [`graph::spawn`] for the shape.

/// The reference plugin and `plugin-server` paths, shared with the
/// integration suites, for unit tests that host a real plugin.
#[cfg(all(test, feature = "plugin"))]
#[path = "../tests/common/plugin.rs"]
mod test_plugin_paths;

mod plugin;

pub mod graph;

// Each `pub mod` below carries its own `//!` header. A `///` here would shadow
// it *and* be resolved in this module's scope, so every intra-doc link in the
// module's header would break.
#[cfg(feature = "midi")]
pub mod midi;

#[cfg(feature = "modulation")]
pub mod modulation;

#[cfg(feature = "audio-io")]
pub mod io;

#[cfg(feature = "sampler")]
pub mod sampler;

#[cfg(feature = "soundfont")]
pub mod soundfont;

/// The polyphonic synth, re-exported whole from `tutti-polysynth`. There is no
/// adapter code: `PolySynth` is an `AudioUnit` spawned like any other node, and
/// its one ECS touchpoint is the `MidiNode` impl beside the trait in
/// `midi::target`.
#[cfg(feature = "synth")]
pub use tutti_polysynth as polysynth;

/// Spatial audio, re-exported whole from `tutti-spatial`. There is no adapter
/// code: the VBAP / binaural panners are plain `AudioUnit`s, and
/// `build_vbap_mix` assembles a subgraph a host spawns like any other node.
#[cfg(feature = "spatial")]
pub use tutti_spatial as spatial;

#[cfg(feature = "plugin")]
pub mod plugin_host;

#[cfg(feature = "export")]
pub mod export;

pub mod engine;

pub use plugin::TuttiPlugin;

// Latency (plugin delay) compensation. Opt-in: `TuttiPlugin` does not add it,
// because it costs a graph walk per commit and a host with no latency-reporting
// nodes never needs it. See the `graph::latency` module docs for ordering.
pub use graph::latency::{ChannelCompensation, GraphLatency, LatencyCompensationPlugin};

#[cfg(feature = "plugin")]
pub use plugin_host::{PluginEmitter, PluginsRes, SetEditorVisible, TuttiHostingPlugin};

// Engine types. The graph itself is `graph::AudioGraphRes`.
pub use engine::{restart_device, restart_device_on, DeviceInfo, DeviceRestart, TuttiDriver};

/// The crate error, at the crate root: its public position and its file
/// position agree, which is the workspace convention.
mod error;
pub use error::{Error, Result};

/// The audio device's UI-facing mirror. Its CPAL driver is this crate's, so the
/// mirror lives here too — in [`engine`], with the rest of the device lifecycle.
pub use engine::AudioDeviceState;

/// Whether the engine is running, and if not, why.
pub use engine::AudioEngineState;

/// Everything a typical host needs, in one import.
pub mod prelude {
    pub use crate::graph::{
        commit_graph, crossfade_audio_node, engine_ready, AudioConfig, AudioGraphRes, AudioParam,
        AudioParamAppExt, AudioPump, AudioPumpAppExt, AudioTapRes, EngineNodes, GraphDirty,
        GraphReconcilePlugin, GraphReconcileSystems, GraphSource, InsertAudioNode, MasterSources,
        MeteringRes, MetronomeRes, PortSource, PortSources, PumpFinished, SpawnAudioNode,
        TransportRes,
    };
    pub use crate::{
        AudioDeviceState, AudioEngineState, ChannelCompensation, DeviceInfo, GraphLatency,
        LatencyCompensationPlugin, TuttiDriver, TuttiPlugin,
    };

    #[cfg(feature = "export")]
    pub use crate::export::{
        ExportClock, ExportDone, ExportError, ExportInFlight, ExportOutput, ExportPlugin,
        ExportRequest, ExportSource, ExportTarget,
    };
    #[cfg(feature = "audio-io")]
    pub use crate::io::{BitDepth, MicIn, MicMonitorNode, Recorder, TapIn, WavOut};
    #[cfg(feature = "midi")]
    pub use crate::midi::{
        MidiBusRes, MidiRoutingRes, MidiSourceInstall, MpeModeRes, TuttiMidiPlugin,
    };
    // `midi-hardware`, not `midi`: gating these with their siblings above breaks
    // a build that takes the software bus without the hardware one.
    #[cfg(feature = "midi-hardware")]
    pub use crate::midi::{MidiDeviceEvent, MidiIoRes};
    #[cfg(feature = "modulation")]
    pub use crate::modulation::{
        ModClock, ModDelivery, ModParamRange, ModRoute, ModSource, ModSourceRate,
        ModTargetRegistry, ModulationMatrix, TuttiModulationPlugin,
    };
    #[cfg(feature = "plugin")]
    pub use crate::plugin_host::{PluginsRes, SetEditorVisible, TuttiHostingPlugin};
    #[cfg(feature = "sampler")]
    pub use crate::sampler::{
        memory_voice, voice_width, DiskStreamerRes, InsertVoice, TuttiPlaybackPlugin,
    };
    #[cfg(feature = "soundfont")]
    pub use crate::soundfont::{
        PlaySoundFont, SoundFontAsset, SoundFontAssetLoader, TuttiSoundFontPlugin,
    };
    #[cfg(feature = "sampler")]
    pub use tutti_sampler::{Playback, Voice, VoiceWindow};

    pub use tutti_core::prelude::*;
    pub use tutti_core::transport::ClickState;
    pub use tutti_core::CrossfadeCurve;

    pub use tutti_core::transport::{
        beat_from_ports, FadeOut, LoopRange, LoopSpan, MetronomeMode, MotionEvent, MotionState,
        Then, BEAT_PORTS,
    };
}

// Test-only global allocator for RT-safety regression tests (relocated from
// the umbrella). Panics on any heap allocation inside `assert_no_alloc(..)`
// scopes. One `#[global_allocator]` per binary — this crate's lib-test binary
// owns it.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
