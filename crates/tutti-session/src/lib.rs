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
//! use tutti_core::{ChannelLayout, Hz, InterleavedMut, SampleRate, Samples};
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
//! `tutti_session::Item`. Nothing here reaches Bevy, and the crate
//! re-exports no crate whole (`scripts/check-bevy-free.sh`).

#![forbid(unsafe_code)]

/// With `plugin`, the main-thread guard of design 015 §4.2 (a no-op until a
/// host marks its main thread); otherwise nothing. Called wherever a retired
/// unit can be freed: the editor's collect, and the session's drop.
#[inline]
pub(crate) fn main_thread() {
    #[cfg(feature = "plugin")]
    tutti_plugin_types::assert_main_thread();
}

mod builder;
#[cfg(feature = "device")]
mod device;
mod error;
mod graph;
#[cfg(feature = "midi")]
mod midi;
#[cfg(feature = "modulation")]
mod modulation;
mod node;
mod session;

pub use builder::{root_width, SessionBuilder};
#[cfg(feature = "device")]
pub use device::{Device, DeviceRestart};
pub use error::{Error, Result};
#[cfg(feature = "modulation")]
pub use graph::ModKeys;
#[cfg(feature = "export")]
pub use graph::{AudioNodeTarget, PendingExport};
pub use graph::{
    AudioSide, Committed, Crossfade, CrossfadeId, CrossfadeRefused, EventSource, Graph,
    GraphSource, ReplaceRefused, LIVE_MAX_BLOCK,
};
#[cfg(feature = "midi")]
pub use midi::{
    drain_receiver_through, LiveInput, Midi, MidiOutDrops, MidiOutRouter, MidiRouteId,
    MidiRouteRule, UmpOut,
};
#[cfg(feature = "modulation")]
pub use modulation::{
    declare_targets, ModRefusal, ModState, Mods, ParamRef, Take, UNRESOLVED_WARNING,
};
pub use node::SessionNode;
#[cfg(feature = "midi")]
pub use session::MidiNodes;
#[cfg(feature = "modulation")]
pub use session::ModInput;
pub use session::{Phase, PhaseCx, Session, SessionEvent, PHASES};
