//! Bevy plugin for the Tutti audio engine.
//!
//! Tutti is a real-time, lock-free audio engine for DAWs and interactive audio:
//! a DSP graph, a transport, MIDI 2.0, sample playback, plugin hosting
//! (VST2/VST3/CLAP/AU), recording and offline export. `bevy-tutti` runs it
//! inside a Bevy `App`: [`TuttiPlugin`] opens the output device and starts the
//! audio callback, each subsystem becomes a Bevy resource, and graph nodes are
//! entities. Wiring, parameters, MIDI routes and modulation are *declared* as
//! components and resources; per-frame reconcile systems write what changed
//! into the graph and publish it to the audio thread once per frame.
//!
//! Use this crate for a Bevy app. For a host without Bevy (a CLI, a server, a
//! renderer), the `tutti` crate re-exports the same engine with no ECS.
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
//!     let osc = commands.spawn_audio_node(ForkByClone(Osc::sine(Hz(440.0)))).id();
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
//!     let (node, _) = graph.insert(tutti_nodes::testing::Osc::sine(tutti_core::Hz(440.0)));
//!     // Staged, not committed: `commit_graph` publishes the frame's edits once.
//!     dirty.0 = true;
//!     let _ = node;
//! }
//! ```
//!
//! A node added this way is **unwired** and renders nothing. What feeds it, and
//! what reaches the speakers, is declared — see [`graph::spawn`] for the shape.
//!
//! # Main types
//!
//! - [`TuttiPlugin`]: opens the device, builds the engine
//!   ([`engine::build_into`]) and adds the subsystem plugins.
//!   [`AudioEngineState`] says whether that worked; [`AudioDeviceState`]
//!   mirrors the device for a UI.
//! - [`graph::AudioGraphRes`]: the editable DSP graph.
//!   [`SpawnAudioNode`](graph::SpawnAudioNode) puts a node on an entity;
//!   [`PortSources`](graph::PortSources) and
//!   [`MasterSources`](graph::MasterSources) declare its audio wiring;
//!   [`AudioParam`](graph::AudioParam) sets a parameter;
//!   [`GraphDirty`](graph::GraphDirty) and
//!   [`commit_graph`](graph::commit_graph) batch a frame's edits into one
//!   commit.
//! - [`graph::TransportRes`], [`graph::MetronomeRes`], [`graph::MeteringRes`]
//!   and [`graph::AudioTapRes`]: the transport, the click, the master meter
//!   and an analysis tap on the master output.
//! - [`GraphLatency`] and [`ChannelCompensation`]: the latency compensation
//!   figures of the plan the audio thread runs.
//! - [`restart_device`]: moves the engine to another device or sample rate.
//! - [`prelude`]: everything a typical host imports.
//!
//! Every system is scheduled in `Update`, ordered by
//! [`GraphReconcileSystems`](graph::GraphReconcileSystems) and gated on
//! [`engine_ready`](graph::engine_ready).
//!
//! # Feature flags
//!
//! No feature is on by default; the default build is the graph, transport,
//! metering and device only.
//!
//! - `full`: every feature below except the plugin formats and `convolution`.
//! - `midi`: MIDI routing, clip sequencing, MIDI files, clock output and MPE,
//!   with no OS MIDI I/O (the `midi` module).
//! - `midi-hardware`: OS MIDI ports, device hot-plug and MIDI 2.0 endpoints;
//!   implies `midi`.
//! - `synth`: the polyphonic synth (`polysynth`, re-exported
//!   `tutti-polysynth`).
//! - `soundfont`: `.sf2` assets and playback (the `soundfont` module);
//!   implies `midi`.
//! - `sampler`: clip playback, disk streaming and the `.wav` asset loader (the
//!   `sampler` module); implies `wav`.
//! - `audio-io`: microphone capture, WAV writing and recording (the `io`
//!   module).
//! - `wav`, `flac`, `mp3`, `ogg`: audio file decoders.
//! - `modulation`: LFOs and a modulation matrix as ECS entities (the
//!   `modulation` module).
//! - `spatial`: VBAP and binaural panners (`spatial`, re-exported
//!   `tutti-spatial`); `hrtf` adds the HRTF binaural panner.
//! - `convolution`: the FFT convolution reverb node.
//! - `export`: offline rendering to files or buffers (the `export` module).
//! - `plugin`: out-of-process plugin hosting, editor windows and catalog scans
//!   (the `plugin_host` module); implies `midi`. `vst2`, `vst3`, `clap` and
//!   `au` enable each plugin format and imply `plugin`.

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

/// The polyphonic synth, re-exported whole from `tutti-polysynth` (feature
/// `synth`). `PolySynth` is a graph node: spawn it with
/// [`spawn_audio_node`](graph::SpawnAudioNode), set its params with an
/// [`AudioParam`](graph::AudioParam), and feed its MIDI event input like any
/// other MIDI-receiving node.
#[cfg(feature = "synth")]
pub use tutti_polysynth as polysynth;

/// Spatial audio, re-exported whole from `tutti-spatial` (feature `spatial`).
/// The VBAP and binaural panners are graph nodes, spawned with
/// [`spawn_audio_node`](graph::SpawnAudioNode) (the binaural one with the
/// `hrtf` feature); their controls land on the entity as
/// [`NodeControls`](graph::NodeControls). `build_vbap_mix` assembles a
/// subgraph into a `tutti_graph::GraphBuilder`.
#[cfg(feature = "spatial")]
pub use tutti_spatial as spatial;

#[cfg(feature = "plugin")]
pub mod plugin_host;

#[cfg(feature = "export")]
pub mod export;

pub mod engine;

pub use plugin::TuttiPlugin;

// What `param_graph_node!` expands to names, reachable from a host crate that
// does not depend on `tutti-graph` itself. Not API.
#[doc(hidden)]
pub mod __private {
    pub use tutti_graph::{ParamNode, ParamSet};
}

// Latency (plugin delay) compensation figures, and the opt-in debug check of
// them. `TuttiPlugin` does not add `LatencyCompensationPlugin`: the graph
// compensates and publishes without it. See the `graph::latency` module docs.
pub use graph::latency::{ChannelCompensation, GraphLatency, LatencyCompensationPlugin};

#[cfg(feature = "plugin")]
pub use plugin_host::{PluginEmitter, PluginsRes, SetEditorVisible, TuttiHostingPlugin};

// Engine types. The graph itself is `graph::AudioGraphRes`.
pub use engine::{restart_device, restart_device_on, DeviceInfo, DeviceRestart, TuttiDriver};

// The crate error, at the crate root: its public position and its file
// position agree, which is the workspace convention.
mod error;
pub use error::{Error, Result};

pub use engine::AudioDeviceState;
pub use engine::AudioEngineState;

/// Everything a typical host needs, in one import.
pub mod prelude {
    pub use crate::graph::{
        commit_graph, crossfade_audio_node, engine_ready, AudioConfig, AudioGraphRes, AudioParam,
        AudioParamAppExt, AudioPump, AudioPumpAppExt, AudioTapRes, EngineNodes, GraphDirty,
        GraphNode, GraphReconcilePlugin, GraphReconcileSystems, GraphSource, InsertAudioNode,
        MasterSources, MeteringRes, MetronomeRes, PortSource, PortSources, PumpFinished,
        SpawnAudioNode, TransportRes,
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
        LiveMidi, LiveMidiInput, MidiEngineNodes, MidiRouteRule, MidiSourceInstall, MpeModeRes,
        TuttiMidiPlugin,
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
    /// A plain node's fork, said at insert: the wrappers that make any
    /// `tutti_graph::Node` a [`crate::graph::GraphNode`].
    pub use tutti_graph::{ForkByClone, Unforkable};

    pub use tutti_core::transport::{
        FadeOut, LoopRange, LoopSpan, MetronomeMode, MotionEvent, MotionState, Then,
    };
}

// Test-only global allocator for RT-safety regression tests. Panics on any heap allocation inside `assert_no_alloc(..)`
// scopes. One `#[global_allocator]` per binary — this crate's lib-test binary
// owns it.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
