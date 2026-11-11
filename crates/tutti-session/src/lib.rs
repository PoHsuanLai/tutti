//! The tutti engine's Bevy-free API layer (design 015): the cross-crate
//! composition both umbrellas use.
//!
//! A [`Session`] is a [`Graph`] (a `tutti-graph` editor, the nodes inserted
//! through it, their event feeds), its transport, and the engine side it
//! renders on, built by a [`SessionBuilder`]: offline (the host pulls blocks
//! with [`Session::render`]), on a device (`device`: the `!Send`
//! `Device` stays on the thread that opened it), or detached (the
//! executor stays on this side). One call, [`Session::update`], runs the
//! steps of a frame in [`PHASES`] order: a Bevy app runs the same phases one
//! system at a time.
//!
//! ```
//! use tutti_types::MaxBlock;
//! use tutti_types::{ChannelLayout, Hz, InterleavedMut, SampleRate, Samples};
//! use tutti_graph::ForkByClone;
//! use tutti_nodes::testing::Osc;
//! use tutti_session::Session;
//!
//! let mut s = Session::builder()
//!     .build_offline(SampleRate(48_000.0), ChannelLayout::MONO, MaxBlock::of(Samples(512)))
//!     .unwrap();
//! let (osc, _) = s.graph().add(ForkByClone(Osc::sine(Hz(1_000.0))));
//! s.graph().set_outputs_from(osc);
//! s.update(); // commits: the graph was edited
//!
//! let mut block = vec![0.0f32; 512];
//! s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::MONO));
//! assert!(block.iter().any(|&x| x > 0.9));
//! ```
//!
//! Like `tutti-graph`, every module is private: everything is
//! `tutti_session::Item`. Nothing here reaches Bevy,
//! and the crate re-exports no crate whole (`cargo xtask check-session`).

#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

/// With `plugin`, the main-thread check of design 015 §4.2 (a `debug_assert!`
/// that is silent until a host claims its main thread); otherwise nothing.
/// Called wherever a retired unit can be freed: the editor's collect, and the
/// session's drop. These paths take no `MainThread` token, so they check.
#[inline]
pub(crate) fn main_thread() {
    #[cfg(feature = "plugin")]
    debug_assert!(
        !tutti_plugin_types::MainThread::claimed_elsewhere(),
        "plugin main-thread call made off the main thread"
    );
}

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
#[cfg(feature = "midi")]
mod instrument;
#[cfg(feature = "midi")]
mod midi;
#[cfg(feature = "modulation")]
mod modulation;
mod node;
#[cfg(feature = "plugin")]
mod plugin;
#[cfg(feature = "record")]
mod recorder;
#[cfg(feature = "plugin")]
mod recovery;
mod session;
#[cfg(feature = "device")]
mod watch;

#[cfg(feature = "export")]
pub use bounce::{
    BOUNCE_STALL_WARNING, Bounce, BounceConfigError, BounceError, BounceHandle, BounceJob,
    BounceLatency, BounceRendered, BounceTail, Bounced, BufferMaster, ExportClock, Field,
    FileMaster, Master, NoMaster, PrepareEdit, PreparedGraph, Range, Stem, StemGain,
};
pub use builder::{SessionBuilder, root_width};
#[cfg(feature = "device")]
pub use device::{Device, DeviceRestart, DeviceRestarted};
pub use edge::{EdgeGainError, EdgeRef, EdgeSink};
pub use error::{ConnectError, Error, HeardTapError, InletError, Result};
#[cfg(feature = "soundfont")]
pub use general_midi::GeneralMidi;
#[cfg(feature = "modulation")]
pub use graph::ModKeys;
pub use graph::RenderWorkerStart;
pub use graph::{
    AudioSide, Committed, Crossfade, CrossfadeId, CrossfadeRefused, EventSource, Feeder, Graph,
    GraphSource, LIVE_MAX_BLOCK, ReplaceRefused,
};
#[cfg(feature = "midi")]
pub use instrument::{InstrumentHandle, NotInstrument, SendError};
#[cfg(feature = "midi")]
pub use midi::{
    LiveInput, MIDI_INPUT_DEVICES, MIDI_INPUT_DROP_REPORT, MIDI_OUT_DROP_REPORT, Midi,
    MidiOutDrops, MidiOutRouter, MidiRouteId, MidiRouteRule, UmpOut, drain_receiver_through,
};
#[cfg(feature = "modulation")]
pub use modulation::{
    MixRef, ModRefusal, ModState, ModTarget, Mods, ParamRef, Take, UNRESOLVED_WARNING,
    declare_targets,
};
pub use node::{Reclaimer, SessionNode};
#[cfg(feature = "plugin")]
pub use plugin::{
    HostedPlugin, PluginLiveness, PluginLoad, PluginLoadRequest, PluginLoaded, PluginReload,
    PluginReloaded, PluginSnapshot, PluginSnapshotTaken, SNAPSHOT_INTERVAL, StateRestore,
};
#[cfg(all(feature = "plugin", feature = "modulation"))]
pub use plugin::{PluginParamRead, PluginParamsRead};
#[cfg(feature = "record")]
pub use recorder::{IDLE_PARK, PumpLoop, PumpPass, Recorder, RecorderError};
#[cfg(feature = "plugin")]
pub use recovery::{GiveUp, Recovery, RecoveryAction, RecoveryEvent, RecoveryPolicy};
#[cfg(feature = "midi")]
pub use session::MidiNodes;
#[cfg(feature = "modulation")]
pub use session::ModInput;
pub use session::{PHASES, Phase, PhaseCx, RootTopologyStatus, Session, SessionEvent};
#[cfg(any(feature = "record", feature = "export"))]
pub use tutti_dsp::DitherShape;
#[cfg(feature = "record")]
pub use tutti_dsp::{DitherSink, DitherSpec};
#[cfg(feature = "export")]
pub use tutti_export::{
    EventLossPolicy, Normalize, Outcome, Produced, Progress, Rendered, Resample, Stage, Written,
};
pub use tutti_graph::{BusError, BusRef, BusSource, Buses, MissingSource};
pub use tutti_graph::{MixError, ModeError, NodeMode, PassError};
#[cfg(feature = "midi")]
pub use tutti_ump::{MidiChannel, MidiGroup};
#[cfg(feature = "device")]
pub use watch::{Backoff, DevicePolicy, DeviceWatch, Follow, FollowEvent, LISTING_WARNING};
