//! Sample playback, disk streaming, and time-stretching for the Tutti audio
//! engine.
//!
//! The entry points are the voices themselves — [`MemorySource`] in memory,
//! [`DiskVoice`] streamed from disk — plus [`VoicePool`] over them and
//! [`DiskStreamer`], the handle that owns the streaming engine. There is no
//! `Sampler` façade type.
//!
//! # Crate layout
//!
//! - [`voice`] — the two playback tiers, the per-track mixer, and the shared
//!   interpolation / placement kernels.
//! - [`stretch`] — phase vocoder (pitch-independent stretch).
//! - [`AudioIn`] / [`AudioOut`] / [`pump`] — the engine's I/O edge vocabulary,
//!   re-exported from `tutti_types::io`.
//!
//! Rates are typed to keep the two tiers honest:
//! [`PlaybackRate`](tutti_core::PlaybackRate) is varispeed (couples pitch),
//! [`SrcRatio`](tutti_core::SrcRatio) is sample-rate conversion (derived, never
//! user intent), and [`StretchFactor`](tutti_core::StretchFactor) drives the
//! phase vocoder (pitch-independent).
//!
//! Streaming is driven through [`DiskStreamer::new`], the
//! [`commands()`](DiskStreamer::commands) WRITE port and the
//! [`status()`](DiskStreamer::status) READ port; that half needs a real file, so
//! its example lives on [`DiskStreamer`] itself as `no_run`.
//!
//! The two-tier rule, the in-memory quick start, the channel ceiling and the
//! features are in the crate README, included below.
#![doc = include_str!("../README.md")]

mod error;
pub use error::{Error, Result};

/// Widest frame the sampler reads, interpolates, or emits.
///
/// Deliberately equal to [`tutti_core::MAX_ROOT_CHANNELS`] — the graph root's
/// own ceiling. **The two move together:** a voice wider than the root can render
/// is a voice nobody can hear, so there is no value in the sampler exceeding it,
/// and letting it do so would mean the truncation happened silently downstream
/// (at the root's fold) rather than visibly here.
///
/// Note the engine has several such ceilings for different paths and they are
/// *not* interchangeable: export folds at 12 (`MAX_NET_CHANNELS`) because an
/// offline render is not bound by the live stack scratch, and the plugin hosts
/// use 16 because a plugin's own bus width is its business.
pub const MAX_SAMPLER_CHANNELS: usize = tutti_core::MAX_ROOT_CHANNELS;

/// Reject the empty layout for anything that is a **graph node**.
///
/// [`ChannelLayout`](tutti_core::ChannelLayout) can represent an empty bus
/// (`Multi(0)`) — deliberately, because a plugin port genuinely can be zero
/// wide. A sampler node cannot: `outputs()` feeds fundsp's graph planner, and a
/// node that reports zero outputs is a node nothing can be wired to. So the
/// widths that reach `AudioUnit::outputs` go through here.
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

#[macro_use]
mod macros;

mod node_id;

// One mock `Timeline` for every test in the crate, replacing three near-identical
// copies whose constructors disagreed on argument order. Test-only.
#[cfg(test)]
mod test_transport;

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

// Time-stretch / pitch-shift (phase vocoder). A peer DSP subsystem, not a
// voice-playback concern: it owns no source and imports nothing from `voice`.
pub mod stretch;

// Bevy-free DSP leaves + value types from `voice` — usable for direct
// FunDSP-graph integration without the ECS layer. The butler's `LruCache` /
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
    Direction, DiskVoice, LoopSetting, MemorySource, MemorySourceConfig, Playback, SlotId, Voice,
    VoiceCommand, VoiceNode, VoiceNodeHandle, VoicePool, VoicePoolHandle, VoiceSource, VoiceWindow,
};
// Entity-as-node markers for the voice pool. The asset loader and the playback
// plugin moved to bevy-tutti (house rule R1); what stays here is the pair of
// marker components, which are derives on this crate's own value types.
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
// service, not a Bevy noun (house rule R2): bevy-tutti wraps it as
// `DiskStreamerRes` and owns `TuttiPlaybackPlugin`.
//
// The live I/O edge — `MicMonitorNode`, `WavOut`, `Recorder` — belongs to
// `tutti-io`, not here, so that `tutti-cpal` (the device layer) need not depend
// on a DSP crate to reach it.
