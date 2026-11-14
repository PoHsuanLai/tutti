#![doc = include_str!("../README.md")]
//! The tutti engine's API layer (design 015): the cross-crate composition.
//!
//! A [`Session`] is a [`Graph`] (a `tutti-graph` editor, the nodes inserted
//! through it, their event feeds), its transport, and the engine side it
//! renders on, built by a [`SessionBuilder`]: offline (the host pulls blocks
//! with [`Session::render`]), on a device (`device`: the `!Send`
//! `Device` stays on the thread that opened it), or detached (the
//! executor stays on this side). One call, [`Session::update`], runs the
//! steps of a frame in [`PHASES`] order.
//!
//! ```
//! use tutti_types::MaxBlock;
//! use tutti_types::{ChannelLayout, Hz, InterleavedMut, SampleRate, Samples};
//! use tutti_graph::ForkByClone;
//! use tutti_nodes::testing::Osc;
//! use tutti_session::{Root, Session};
//!
//! let mut s = Session::builder()
//!     .outputs_floor(ChannelLayout::MONO)
//!     .build_offline(SampleRate::SR_48K, ChannelLayout::MONO, MaxBlock::of(Samples(512)))
//!     .unwrap();
//! let (osc, _) = s.graph().add(ForkByClone(Osc::sine(Hz(1_000.0))));
//! s.graph().connect(osc, Root).unwrap();
//! s.update(); // commits: the graph was edited
//!
//! let mut block = vec![0.0f32; 512];
//! s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::MONO));
//! assert!(block.iter().any(|&x| x > 0.9));
//! ```
//!
//! # The two loops
//!
//! The control thread owns the [`Session`]. Each frame it calls
//! [`Session::update`], which runs the [`Phase`]s of [`PHASES`] in order and
//! returns the [`SessionEvent`]s they raised: [`Phase::Collect`] lands what
//! the session's jobs sent and frees what the audio thread retired,
//! [`Phase::Commit`] compiles and sends an edited graph, and the phases around
//! them serve plugins, modulation, MIDI and reclamation.
//!
//! The audio thread runs the other loop: every block it renders the newest
//! plan it has been handed, without waiting for the control thread.
//!
//! Between the two there are queues and no mutable state. A commit travels
//! to the audio thread over the editor's queue, retired values travel back
//! over its return queue, and a job on the session's spawner reports over a
//! channel that [`Phase::Collect`] drains. A call on the [`Graph`] only stages
//! an edit, in the graph value the control thread holds
//! ([`Graph::topology`], [`Graph::is_edited`]); nothing reaches the audio
//! thread until an update's commit sends it.
//!
//! Like `tutti-graph`, every module is private: everything is
//! `tutti_session::Item`. The crate re-exports no crate whole (`cargo xtask check-session`).

#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(feature = "export")]
mod bounce;
mod builder;
#[cfg(feature = "device")]
mod device;
mod edge;
mod error;
#[cfg(feature = "export")]
mod fork;
#[cfg(feature = "soundfont")]
mod general_midi;
mod graph;
#[cfg(all(feature = "device", feature = "audio-io"))]
mod input;
#[cfg(feature = "midi")]
mod instrument;
mod jobs;
mod key;
#[cfg(feature = "plugin")]
mod main_session;
#[cfg(feature = "midi")]
mod midi;
#[cfg(feature = "midi")]
mod midi_event;
#[cfg(feature = "midi")]
mod midi_follow;
#[cfg(feature = "midi-hardware")]
mod midi_hardware;
#[cfg(feature = "modulation")]
mod modulation;
mod node;
mod params;
#[cfg(feature = "plugin")]
mod plugin;
#[cfg(feature = "plugin")]
mod plugin_diagnostics;
#[cfg(feature = "plugin")]
mod plugin_params;
#[cfg(feature = "plugin")]
mod plugin_presets;
#[cfg(feature = "record")]
mod recorder;
#[cfg(feature = "plugin")]
mod recovery;
#[cfg(feature = "sampler")]
mod sampler;
mod session;
#[cfg(all(feature = "device", feature = "audio-io"))]
mod take;
#[cfg(feature = "device")]
mod watch;

#[cfg(feature = "export")]
pub use bounce::{
    AnyStem, BOUNCE_STALL_WARNING, Bounce, BounceConfigError, BounceError, BounceHandle, BounceJob,
    BounceLatency, BounceRendered, BounceTail, Bounced, BufferMaster, BufferStem, ExportClock,
    FileMaster, FileStem, Master, NoMaster, PrepareEdit, PreparedGraph, Range, Stem, StemGain,
};
pub use builder::{SessionBuilder, Workers, root_width};
#[cfg(feature = "device")]
pub use device::{
    Device, DeviceEvent, DeviceLatency, DeviceRestart, DeviceRestarted, LiveSession, RestartError,
    RestartOutcome,
};
pub use edge::EdgeGainError;
pub use error::{
    ConnectError, Error, HeardTapError, InletError, NoSuchNode, RemoveError, Result, ScheduleError,
    TempoError,
};
#[cfg(feature = "midi")]
pub use error::{MidiError, RouteDeclarer};
#[cfg(feature = "soundfont")]
pub use general_midi::GeneralMidi;
#[cfg(feature = "modulation")]
pub use graph::ModKeys;
pub use graph::RenderWorkerStart;
#[cfg(feature = "testing")]
pub use graph::{AudioSide, Testing};
pub use graph::{
    Committed, Crossfade, CrossfadeId, CrossfadeRefused, Feeder, Graph, LIVE_MAX_BLOCK, Meters,
    ReplaceRefused, Taps,
};
#[cfg(all(feature = "device", feature = "audio-io"))]
pub use input::{Input, InputConfig, InputOpener, OpenedInput, Trim};
#[cfg(feature = "midi")]
pub use instrument::{AddedInstrument, AnyNode, InstrumentHandle, NotInstrument};
pub use jobs::{Job, JobFailed};
pub use key::{
    Bus, BusSink, Channel, Connection, Edge, Event, EventInPort, EventOutPort, EventTapPort, Fanin,
    InPort, Inlet, Key, MainBus, MainSink, OutPort, Outlet, Root, Weighted, Wire,
};
#[cfg(feature = "plugin")]
pub use main_session::{EditorView, MainPlugin, MainSession, Opened};
#[cfg(feature = "midi")]
pub use midi::{
    MIDI_INPUT_DEVICES, MIDI_INPUT_DROP_REPORT, MIDI_OUT_DROP_REPORT, Midi, MidiLoad, MidiOutDrops,
    MidiOutRouter, MidiRouteId, MidiRouteRule, NoSuchMidiRoute, OutputTiming, TakePlacement,
    UmpOut, drain_receiver_through,
};
#[cfg(feature = "midi")]
pub use midi_event::MidiEvent;
#[cfg(feature = "midi")]
pub use midi_follow::{FollowClock, FollowMachineControl, FollowTempo, SendMachineControl};
#[cfg(feature = "midi-hardware")]
pub use midi_hardware::Resolution;
#[cfg(feature = "modulation")]
pub use modulation::{
    AutomationTake, MixRef, ModRefusal, ModRouteId, ModSourceId, ModState, ModTarget, Mods,
    ModsView, NoAutomation, NoSuchModRoute, NoSuchModSource, ParamRef, Problem, Ranged, RouteError,
    RouteInfo, RouteMut, RouteSpec, SourceInfo, SourceName, TargetName, UNRESOLVED_WARNING,
    declare_targets,
};
pub use node::{Reclaimer, SessionNode};
pub use params::{ParamError, RangeError, TypedParamRef};
#[cfg(feature = "plugin")]
pub use plugin::{
    Plugin, PluginChange, PluginEvent, PluginHealth, PluginLoad, PluginLoadRequest, PluginLoaded,
    PluginNode, SNAPSHOT_INTERVAL, StateRestore, StateView,
};

#[cfg(feature = "plugin")]
pub use plugin_diagnostics::{Diagnostics, PluginBuses};
#[cfg(feature = "plugin")]
pub use plugin_params::{Param, ParamGesture, Params, PluginParamError, TypedParam};
#[cfg(feature = "plugin")]
pub use plugin_presets::{PresetEntry, PresetKind, Presets};
#[cfg(feature = "record")]
pub use recorder::{Finite, IDLE_PARK, Live, PumpLoop, PumpPass, Recorder, RecorderError};
#[cfg(feature = "plugin")]
pub use recovery::{
    GiveUp, PortCounts, Recovery, RecoveryAction, RecoveryEvent, RecoveryFailure, RecoveryPolicy,
    ReloadCause,
};
#[cfg(feature = "sampler")]
pub use sampler::{Residency, STREAM_ABOVE_BYTES, SamplerError, SamplerEvent, SamplerLoad};
#[cfg(feature = "midi")]
pub use session::MidiNodes;
#[cfg(feature = "modulation")]
pub use session::ModInput;
pub use session::{PHASES, Phase, PhaseCx, RootTopologyStatus, Session, SessionEvent};
#[cfg(all(feature = "device", feature = "audio-io"))]
pub use take::{
    Armed, InFlight, Placement, Punch, PunchError, Recorded, Rolling, Start, Take, TakeError,
    TakeEvent, TakeId, TakeTarget, Terms, Window,
};
#[cfg(any(feature = "record", feature = "export"))]
pub use tutti_dsp::DitherShape;
#[cfg(feature = "record")]
pub use tutti_dsp::{DitherSink, DitherSpec};
pub use tutti_engine::{HeardOpen, Playhead};
#[cfg(feature = "export")]
pub use tutti_export::{
    EventLossPolicy, Normalize, Outcome, Produced, Progress, Rendered, Resample, Stage, Written,
};
pub use tutti_graph::{
    BusError, BusRef, BusSource, Buses, HasParam, HasParams, MissingSource, ParamUnit, Scalar,
};
pub use tutti_graph::{
    CloseError, CommitError, EventCapacity, Losses, MixError, ModeError, NodeMode, PassError, Stamp,
};
#[cfg(feature = "midi")]
pub use tutti_ump::{MidiChannel, MidiGroup};
#[cfg(feature = "device")]
pub use watch::{Backoff, DevicePolicy, DeviceWatch, Follow, FollowEvent, LISTING_WARNING};
