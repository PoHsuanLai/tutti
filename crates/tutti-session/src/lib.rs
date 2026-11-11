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
//! use tutti_types::{ChannelLayout, Hz, InterleavedMut, SampleRate, Samples};
//! use tutti_graph::ForkByClone;
//! use tutti_nodes::testing::Osc;
//! use tutti_session::Session;
//!
//! let mut s = Session::builder()
//!     .build_offline(SampleRate(48_000.0), ChannelLayout::MONO, Samples(512))
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
//! `tutti_session::Item`. Nothing here reaches Bevy
//! (`scripts/check-bevy-free.sh`), and the crate re-exports no crate whole
//! (`scripts/check-adapter.sh`).

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

mod builder;
#[cfg(feature = "device")]
mod device;
mod edge;
mod error;
#[cfg(feature = "export")]
mod export;
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

pub use builder::{SessionBuilder, root_width};
#[cfg(feature = "device")]
pub use device::{Device, DeviceRestart, DeviceRestarted};
pub use edge::{EdgeGainError, EdgeRef, EdgeSink};
pub use error::{Error, HeardTapError, InletError, Result};
#[cfg(feature = "export")]
pub use export::{
    EXPORT_STALL_WARNING, ExportClock, ExportHandle, ExportJob, ExportOutput, ExportRendered,
    ExportRequest, ExportSource, ExportTarget, PrepareEdit, PreparedGraph,
};
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
    LiveInput, MIDI_INPUT_DROP_REPORT, MIDI_OUT_DROP_REPORT, Midi, MidiOutDrops, MidiOutRouter,
    MidiRouteId, MidiRouteRule, UmpOut, drain_receiver_through,
};
#[cfg(feature = "modulation")]
pub use modulation::{
    ModRefusal, ModState, ModTarget, Mods, ParamRef, Take, UNRESOLVED_WARNING, declare_targets,
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
pub use tutti_graph::MissingSource;
#[cfg(feature = "midi")]
pub use tutti_ump::MidiChannel;
#[cfg(feature = "device")]
pub use watch::{Backoff, DevicePolicy, DeviceWatch, Follow, FollowEvent, LISTING_WARNING};
