#![doc = include_str!("../README.md")]
//!
//! ## Main types
//!
//! - [`MemorySource`]: in-memory playback of a [`Wave`], configured with
//!   [`MemorySourceConfig`], [`LoopSetting`] and [`VoiceWindow`].
//! - [`DiskVoice`]: disk-streamed playback, built by [`Status::take_disk_voice`].
//! - [`DiskStreamer`]: owns the butler thread; [`Commands`] (a stream
//!   [`Command`]) in, [`Status`] out.
//! - [`Voice`], [`VoiceSource`], [`Playback`]: a voice of either tier and its
//!   control state.
//! - [`VoicePool`] with [`VoicePoolHandle`], and [`VoiceNode`] with
//!   [`VoiceNodeHandle`]: the graph nodes that play voices, and their
//!   control-thread handles ([`VoiceCommand`]).
//! - [`stretch::Unit`]: the phase-vocoder time stretch and pitch shift.
//! - [`Source`] and `probe`: the tier choice and the header probe that
//!   informs it.
//! - [`voice::interp`]: the interpolation kernel and transport-placement
//!   helpers the voices share.
//! - [`AudioIn`], [`AudioOut`], [`pump`]: the engine's I/O edge vocabulary,
//!   re-exported from `tutti-core` because the butler's refill path speaks it.
//!
//! ## License
//!
//! MIT OR Apache-2.0.

mod error;
pub use error::{Error, Result};

/// Widest frame the sampler reads, interpolates, or emits, in channels.
///
/// Equal to [`tutti_core::MAX_ROOT_CHANNELS`], the graph root's own ceiling,
/// and the two move together: a voice wider than the root can render is a
/// voice nobody can hear, and letting the sampler exceed it would move the
/// truncation silently downstream (to the root's fold) rather than reporting
/// it here ([`PoolTooWide`]).
pub const MAX_SAMPLER_CHANNELS: usize = tutti_core::MAX_ROOT_CHANNELS;

/// Reject the empty layout for anything that is a **graph node**.
///
/// [`ChannelLayout`](tutti_core::ChannelLayout) can represent an empty bus
/// (`Multi(0)`) — deliberately, because a plugin port genuinely can be zero
/// wide. A sampler node cannot: a node that reports zero outputs is a node
/// nothing can be wired to. So the widths a node's `Shape` declares go
/// through here.
///
/// This is the one place that clamp lives: the layout carries the declaration
/// and this carries the node-arity invariant, so no call site has to re-remember
/// the rule. The upper bound is *not* applied here — [`MAX_SAMPLER_CHANNELS`] is
/// a per-read stack ceiling, not a limit on what a node may declare, and only
/// [`DiskVoice`] needs it.
#[inline]
pub(crate) fn nonempty(layout: tutti_core::ChannelLayout) -> tutti_core::ChannelLayout {
    if layout.count() == 0 {
        tutti_core::ChannelLayout::MONO
    } else {
        layout
    }
}

// One mock transport and block driver for every test of the crate, in-crate
// and in `tests/` (through `test-support`): a node reads the transport from
// its block's `Env`, and this is what hands it one.
#[cfg(any(test, feature = "test-support"))]
pub mod testing;

// The I/O edge vocabulary is defined once in `tutti-types` and re-exported by
// `tutti-core`. Re-exported again here because the butler's refill path speaks
// it: `FileIn` is the `AudioIn` it polls, and the region ring (`RegionOut`) is
// itself an `AudioOut`, both carrying their width as a runtime `ChannelLayout`.
pub use tutti_core::io::{pump, AudioIn, AudioOut, OnEmpty};

// The resident buffer every voice constructor takes (`MemorySource::new`,
// `Voice`'s memory tier) is `tutti-io`'s. Re-exported so a consumer building a
// voice needs no direct `tutti-io` dependency. `WaveAsset` is not here: it
// exists only under `tutti-io/bevy`, which this crate does not enable — it is
// `bevy_tutti::sampler`'s to surface.
pub use tutti_io::Wave;

// Voice playback: the two tier units, the mixer over them, and the kernels they
// share. Bevy-free apart from the asset loader, gated inside.
pub mod voice;

// Planar block scratch and its kernels: the voices render into it, the
// stretch filter reads and writes it. Crate-level because both use it.
mod lanes;

// Time-stretch / pitch-shift (phase vocoder). A peer DSP subsystem, not a
// voice-playback concern: it owns no source and imports nothing from `voice`.
pub mod stretch;

// Bevy-free DSP leaves + value types from `voice` — usable as plain
// `tutti_graph` nodes without the ECS layer. The butler's `LruCache` /
// `StreamPin` are internal machinery a consumer never constructs, so they stay
// `pub(crate)`.
// `DiskVoiceConfig` and `DiskSource` are deliberately NOT re-exported: nothing
// outside this crate constructs them. `DiskSource` in particular is
// `DiskVoice`'s `inner` — one capability, and only the outer type is a doorway.
//
// `voice` itself stays PUBLIC, unlike most modules in the engine: its submodules
// (`interp`, `types`, `command`, `pool`, `node`, `memory_source`) cross-link
// heavily in their own docs, and privatizing it turns 31 of those into dangling
// references. It is a real internal namespace, not a redundant path.
pub use voice::{
    Direction, DiskVoice, DiskVoiceControls, LoopSetting, MemorySource, MemorySourceConfig,
    Playback, PoolTooWide, SlotId, Voice, VoiceCommand, VoiceNode, VoiceNodeHandle, VoicePool,
    VoicePoolHandle, VoiceSource, VoiceWindow,
};
// Entity-as-node markers for the voice pool. The asset loader and the playback
// plugin are bevy-tutti's; what lives here is the pair of marker components,
// which are derives on this crate's own value types.
#[cfg(feature = "bevy")]
pub use voice::{VoicePoolNode, VoicePoolRef};

// The async disk-streaming engine (butler thread + the `DiskStreamer` handle). All
// Bevy-free — a non-Bevy host drives it directly via `DiskStreamer::new` / the
// `ButlerThread` API.
pub(crate) mod butler;

pub use butler::{DiskStreamer, DiskStreamerConfig, TakeVoiceError};

// The hand-driven butler cycle's verdict, alongside `DiskStreamer::manual`.
#[cfg(any(test, feature = "test-support"))]
pub use butler::StepOutcome;

mod ports;
pub use ports::{Command, Commands, Source, Status};

// Header-only probe: what a file is, and whether *this build's* butler can
// stream it. Public because the tier decision is the caller's
// (`Source`'s doc: "the sampler never decides the tier on its own") but the
// streamability half of it is this crate's own capability — so the host and the
// butler read one function rather than two copies of a rule.
//
// Codec-gated because it *is* the codec layer: `Wave::probe_metadata` and
// `WaveMetadata` are themselves gated in `tutti-io`, so with no format feature
// there is no header to read. Absent rather than always-`false` — a host that
// compiled out every codec cannot open files at all, and a probe that silently
// answered "not streamable" would look like a property of the file.
#[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
mod probe;
#[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
pub use probe::{probe, ProbeError, SampleFacts};

// This crate exposes no Bevy plugin of its own. `DiskStreamer` is an engine
// service, not a Bevy noun: bevy-tutti wraps it as `DiskStreamerRes` and owns
// `TuttiPlaybackPlugin`.
//
// The live I/O edge — `MicMonitorNode`, `WavOut`, `Recorder` — belongs to
// `tutti-io`, not here, so that `tutti-cpal` (the device layer) need not depend
// on a DSP crate to reach it.
